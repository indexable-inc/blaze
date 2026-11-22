//! Positions of file items and members come from the current FileItems row.
//! Expansion spans point into the expansion's own checked text.

use blaze_id::ItemKey;
use blaze_scope::items_record::{ItemRow, ItemsRecordError, MemberRow, TextSpan};
use blaze_scope::resolve::At;
use blaze_surface::ItemIndex;

use super::{Log, Records, SourceError};

impl<'n> Records<'n> {
    pub(super) fn row_now(&'n self, krate: usize, key: ItemKey) -> Option<ItemRow<'n>> {
        let row = self.read(krate, self.items_of(krate)?.item(key))??;
        let Some(span) = row.text else {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: ItemsRecordError::NoPlace { key } });
            return None;
        };
        if matches!(span.at, At::Expansion(_)) {
            self.read(krate, self.items.whole(krate, span.at))?;
        }
        Some(row)
    }

    pub(super) fn span_now(&'n self, krate: usize, span: TextSpan) -> Option<&'n str> {
        self.read(krate, self.items.text(krate, span))
    }

    pub(super) fn member_now(&'n self, krate: usize, owner: ItemKey, member: &MemberRow<'n>) -> Option<(ItemIndex, &'n str)> {
        let text = self.read(krate, self.items.member_text(owner, member))?;
        Some((member.outline, text))
    }

    pub fn file_now(&'n self, krate: usize, file: u32) -> Option<&'n str> {
        self.items_of(krate)?;
        self.try_file_now(krate, file).ok()
    }

    /// Read the current file, preserving the refusal for a driver that can schedule
    /// a changed surface. Other consumers of this job still see it in the records log.
    pub fn try_file_now(&'n self, krate: usize, file: u32) -> Result<&'n str, ItemsRecordError> {
        self.items.file_now(krate, file).inspect_err(|error| {
            self.log.put(SourceError::Items { krate, name: self.log.name(krate), error: error.clone() });
        })
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
