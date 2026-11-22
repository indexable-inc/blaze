//! A crate's expansion directory. Expansion text records are opened only when a front2 load
//! asks for their keyed record, so opening a crate's records never reads every expansion row.

use blaze_id::ItemKey;
use blaze_scope::expansion_origins::CallView;
use blaze_scope::expansion_records::{Directory, Entry, ExpansionRecordsError};

/// A crate's expansion directory read in place. Only the loader follows an entry's record key.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExpansionTexts<'s> {
    directory: Directory<'s>,
}

impl<'s> ExpansionTexts<'s> {
    pub(crate) fn open(bytes: &'s [u8]) -> Result<ExpansionTexts<'s>, ExpansionRecordsError> {
        Directory::open(bytes).map(|directory| ExpansionTexts { directory })
    }

    pub(crate) fn len(&self) -> usize {
        self.directory.len()
    }

    pub(crate) fn call_at(&self, at: usize) -> Result<ItemKey, ExpansionRecordsError> {
        Ok(self.directory.entry_at(at)?.call)
    }

    pub(crate) fn entry(&self, call: ItemKey) -> Result<Option<Entry<'s>>, ExpansionRecordsError> {
        self.directory.entry(call)
    }

    pub(crate) fn call(&self, call: ItemKey) -> Result<Option<Option<CallView<'s>>>, ExpansionRecordsError> {
        let Some(entry) = self.entry(call)? else { return Ok(None) };
        if entry.call_part.is_empty() {
            return Ok(Some(None));
        }
        CallView::open(entry.call_part).map(|view| Some(Some(view))).map_err(|error| ExpansionRecordsError::Origins { call, error })
    }
}

