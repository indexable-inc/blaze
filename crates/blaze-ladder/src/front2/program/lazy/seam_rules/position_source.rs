//! Reverse lookups on the two representations of source declarations. These
//! operate on names and item rows, independently of which bodies have run.

use super::{LibKey, Names, NamesSource, PositionGap, TextAt, checker_of};
use crate::front2::source::Records;
use blaze_surface::{ItemDetail, ItemKind};
use blaze_typeck::ItemKey;

pub trait PositionSource<'n>: NamesSource<'n> {
    fn member_beginning(self, krate: usize, item: ItemKey) -> Result<Option<LibKey>, PositionGap>;
    /// Only declarations whose body can contain an item. Reading these keys
    /// does not open the text of an unrelated body.
    fn enclosing_keys(self, krate: usize, visit: impl FnMut(LibKey) -> Result<(), PositionGap>) -> Result<(), PositionGap>;
    fn expansion_text(self, krate: usize, call: LibKey) -> Option<&'n str>;
}

impl<'n> PositionSource<'n> for &'n Names<'n> {
    fn member_beginning(self, krate: usize, item: ItemKey) -> Result<Option<LibKey>, PositionGap> {
        let c = self.crates().get(krate).ok_or(PositionGap::NoItem)?;
        let mut rows = c.tables.members.values().flat_map(|ms| ms.iter())
            .filter(|m| m.name.is_some() && checker_of(m.key) == item);
        let first = rows.next().map(|m| m.key);
        if rows.next().is_some() { Err(PositionGap::NoItem) } else { Ok(first) }
    }

    fn enclosing_keys(self, krate: usize, mut visit: impl FnMut(LibKey) -> Result<(), PositionGap>) -> Result<(), PositionGap> {
        let c = self.crates().get(krate).ok_or(PositionGap::NoItem)?;
        let holds = |detail| matches!(detail, ItemDetail::Fn { body_holds_items: true }
            | ItemDetail::Const { body_holds_items: true } | ItemDetail::Static { body_holds_items: true });
        for (lib, _, place) in self.items(krate) {
            let (_, outline) = self.outline_at(krate, place.at).ok_or(PositionGap::NoItem)?;
            if holds(outline.item(place.item).detail) {
                visit(lib)?;
            }
        }
        for members in c.tables.members.values() {
            for member in members.iter().filter(|m| holds(m.detail)) {
                visit(member.key)?;
            }
        }
        Ok(())
    }

    fn expansion_text(self, krate: usize, call: LibKey) -> Option<&'n str> {
        Some(self.outline_at(krate, TextAt::Expansion(call))?.0)
    }
}

impl<'n> PositionSource<'n> for &'n Records<'n> {
    fn member_beginning(self, krate: usize, item: ItemKey) -> Result<Option<LibKey>, PositionGap> {
        // A failed item-prefix read invalidates the lookup, even when a
        // different table has a row beginning with those bytes.
        if !self.errors().is_empty() {
            return Err(PositionGap::NoItem);
        }
        let record = self.items_record(krate).ok_or(PositionGap::NoItem)?;
        let get = |i| record.owned_key_at(i).map_err(|_| PositionGap::NoItem);
        let (mut lo, mut hi) = (0, record.owned_members());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if checker_of(get(mid)?.ok_or(PositionGap::NoItem)?) < item {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let first = get(lo)?.filter(|k| checker_of(*k) == item);
        if first.is_some() && get(lo + 1)?.is_some_and(|k| checker_of(k) == item) {
            return Err(PositionGap::NoItem);
        }
        Ok(first)
    }

    fn enclosing_keys(self, krate: usize, mut visit: impl FnMut(LibKey) -> Result<(), PositionGap>) -> Result<(), PositionGap> {
        let record = self.items_record(krate).ok_or(PositionGap::NoItem)?;
        for i in 0..record.items() {
            let row = record.item_at(i).map_err(|_| PositionGap::NoItem)?.ok_or(PositionGap::NoItem)?;
            if row.body_holds_items && matches!(row.kind, ItemKind::Function | ItemKind::Const | ItemKind::Static) {
                visit(row.key)?;
            }
            if matches!(row.kind, ItemKind::Impl | ItemKind::Trait)
                && let Some(members) = record.members(row.key).map_err(|_| PositionGap::NoItem)?
            {
                for member in members.iter() {
                    let member = member.map_err(|_| PositionGap::NoItem)?;
                    if member.body_holds_items && matches!(member.kind, ItemKind::Function | ItemKind::Const) {
                        visit(member.key)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn expansion_text(self, krate: usize, call: LibKey) -> Option<&'n str> {
        self.whole_text(krate, TextAt::Expansion(call))
    }
}
