//! The typed stops of the item layer: what a question could not be answered with, and what a
//! record builder refused.

use core::fmt;

use blaze_id::{ContentId, CrateItemKey, ItemKey, NoItem};

/// A question the new layer cannot answer yet (docs/v2/plan/design/bottom.md, "How it is built
/// and proven"): which method, about which item. The stops a run counts are the work list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NotBuilt {
    pub method: &'static str,
    pub item: ItemKey,
}

impl fmt::Display for NotBuilt {
    /// The item in full, so it can be searched for in a profile or a record.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not built: {} of {:x}", self.method, self.item)
    }
}

impl core::error::Error for NotBuilt {}

/// Why a question about an item has no answer.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SurfaceError {
    /// The layer cannot answer this question yet.
    NotBuilt(NotBuilt),
    /// The key names no item of its crate.
    NoItem(NoItem),
    /// The item exists, and the record holding `fact` about it is not there: it is made by the
    /// item's own crate's task, never by the asker (bottom.md, point 5).
    NoRecord { fact: &'static str, item: ItemKey },
    /// No record holds `fact` of the file with this content: no [`crate::FileItems`] of it, or
    /// no text for a [`crate::Source`] question.
    NoFileRecord { fact: &'static str, file: ContentId },
    /// No record holds `fact` of the crate: its root record, lang items, graph or impls.
    NoCrateRecord { fact: &'static str, krate: CrateItemKey },
    /// A [`crate::HasText`] of one crate was handed to the [`crate::Source`] of another.
    WrongCrate { proof: CrateItemKey, krate: CrateItemKey },
    /// A [`crate::Local`] no number of this task's [`crate::LocalTable`] is: one of another
    /// task's table.
    NoLocal { local: u32 },
    /// A task's [`crate::LocalTable`] would pass `u32::MAX` items.
    LocalsFull,
}

impl From<NotBuilt> for SurfaceError {
    fn from(stop: NotBuilt) -> SurfaceError {
        SurfaceError::NotBuilt(stop)
    }
}

impl From<NoItem> for SurfaceError {
    fn from(missing: NoItem) -> SurfaceError {
        SurfaceError::NoItem(missing)
    }
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SurfaceError::NotBuilt(stop) => fmt::Display::fmt(stop, f),
            SurfaceError::NoItem(missing) => fmt::Display::fmt(missing, f),
            SurfaceError::NoRecord { fact, item } => {
                write!(f, "no record of {fact} of the item {item:x}")
            }
            SurfaceError::NoFileRecord { fact, file } => {
                write!(f, "no record of {fact} of the file {file:x}")
            }
            SurfaceError::NoCrateRecord { fact, krate } => {
                write!(f, "no record of {fact} of the crate {krate:x}")
            }
            SurfaceError::WrongCrate { proof, krate } => write!(
                f,
                "a proof of holding the text of the crate {proof:x} was handed to the source of the crate {krate:x}"
            ),
            SurfaceError::NoLocal { local } => {
                write!(f, "the local item {local} is no number of this task's table")
            }
            SurfaceError::LocalsFull => f.write_str("a task's table of local items would pass u32::MAX"),
        }
    }
}

impl core::error::Error for SurfaceError {}

/// Why a record builder refused what it was given.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BuildError {
    /// `table` would hold more than `u32::MAX` rows or bytes.
    TooLarge { table: &'static str },
    /// A row of `table` names `index`, which is not a row it may name: an item index past the
    /// file's items, or a parent that is not before its child.
    BadIndex { table: &'static str, index: u32 },
    /// A byte range of the item at `index` runs past the item's own length.
    PastItem { index: u32 },
    /// `table` would hold two rows for one slot: two bindings of one namespace and name, two
    /// places for one key, two lang items of one name, two crates of one key. `key` is the
    /// second row's item or crate.
    Twice { table: &'static str, key: ContentId },
    /// A row of `table` names a crate the graph does not hold.
    Unknown { table: &'static str, key: ContentId },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::TooLarge { table } => {
                write!(f, "the table {table} would pass u32::MAX rows or bytes")
            }
            BuildError::BadIndex { table, index } => {
                write!(f, "a row of the table {table} names the row {index}, which it may not")
            }
            BuildError::PastItem { index } => {
                write!(f, "a byte range of item {index} runs past the item's length")
            }
            BuildError::Twice { table, key } => {
                write!(f, "the table {table} would hold two rows for one slot, the second of {key:x}")
            }
            BuildError::Unknown { table, key } => {
                write!(f, "a row of the table {table} names the crate {key:x}, which the graph does not hold")
            }
        }
    }
}

impl core::error::Error for BuildError {}
