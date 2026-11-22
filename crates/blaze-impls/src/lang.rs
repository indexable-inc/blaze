//! [`lang_item`]: one lang item found across the crates a crate loads, over the `LangItems`
//! records (`blaze_surface::LangItemsView`).
//!
//! Across the graph, rustc loads every crate's lang items and refuses a name defined twice
//! (E0152). [`lang_item`] reads, in index order, the crates `from` loads that may define
//! lang items ([`CrateFlags::LANG_ITEMS`], from each crate's roots: the language's crates and
//! those with `#![feature(lang_items)]`), typically `core`, `alloc` and `std`; every read is
//! noted, and a second definition is [`LangError::Duplicate`], naming both.

use core::fmt;

use blaze_format::FormatError;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_surface::{CrateFlags, CrateGraphView, CrateIndex, LangItemsView, LangName};

/// Where a lang item lookup gets each crate's `LangItems` bytes (aligned to
/// `blaze_format::ALIGN`), and is told of each read.
pub trait LangRecords {
    type Error;

    fn lang_items(&self, krate: CrateItemKey) -> Result<&[u8], Self::Error>;

    fn note(&self, read: LangRead<'_>);
}

/// A read of a lang item lookup: `krate` was asked for `name`, and answered `found`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LangRead<'t> {
    pub krate: CrateItemKey,
    pub name: LangName<'t>,
    pub found: Option<ItemKey>,
}

/// A lang item: the crate defining it and the item.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LangItem {
    pub krate: CrateItemKey,
    pub item: ItemKey,
}

/// Why a lang item lookup stopped.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LangError<'t, E> {
    /// Two crates `from` loads define `name` (E0152): the first by index order, and the other.
    Duplicate { name: LangName<'t>, first: LangItem, second: LangItem },
    Records { krate: CrateItemKey, error: E },
    Format { krate: CrateItemKey, error: FormatError },
    /// The record read for `krate` is of `found`.
    WrongCrate { krate: CrateItemKey, found: CrateItemKey },
}

impl<E: fmt::Display> fmt::Display for LangError<'_, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LangError::Duplicate { name, first, second } => write!(
                f,
                "the lang item `{name}` is defined twice: as {:x} in the crate {:x} and as {:x} in the crate {:x} (E0152)",
                first.item, first.krate, second.item, second.krate
            ),
            LangError::Records { krate, error } => {
                write!(f, "the LangItems record of the crate {krate:x} could not be read: {error}")
            }
            LangError::Format { krate, error } => {
                write!(f, "the LangItems record of the crate {krate:x} is not one: {error}")
            }
            LangError::WrongCrate { krate, found } => {
                write!(f, "the LangItems record read for the crate {krate:x} is of the crate {found:x}")
            }
        }
    }
}

impl<E: fmt::Display + fmt::Debug> core::error::Error for LangError<'_, E> {}

/// The lang item `name` as `from` sees it: its one definition among the crates `from` loads,
/// none when no crate defines it, [`LangError::Duplicate`] when two do.
pub fn lang_item<'t, C: LangRecords>(
    graph: &CrateGraphView<'_>,
    from: CrateIndex,
    name: LangName<'t>,
    cx: &C,
) -> Result<Option<LangItem>, LangError<'t, C::Error>> {
    let mut found: Option<LangItem> = None;
    for at in graph.closure(from) {
        if !graph.flags(at).contains(CrateFlags::LANG_ITEMS) {
            continue;
        }
        let krate = graph.key(at);
        let bytes = cx.lang_items(krate).map_err(|error| LangError::Records { krate, error })?;
        let view = LangItemsView::read(bytes).map_err(|error| LangError::Format { krate, error })?;
        if view.krate() != krate {
            return Err(LangError::WrongCrate { krate, found: view.krate() });
        }
        let item = view.get(name);
        cx.note(LangRead { krate, name, found: item });
        let Some(item) = item else { continue };
        let here = LangItem { krate, item };
        match found {
            Some(first) => return Err(LangError::Duplicate { name, first, second: here }),
            None => found = Some(here),
        }
    }
    Ok(found)
}
