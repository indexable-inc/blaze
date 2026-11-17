//! A survey's left-out row as DATA (rule 19: an error names the thing; typed errors, never a
//! string in place of one): the fn's typed refusal itself, the class a census groups it by,
//! what splits that class into the causes behind it, and where in an item the refusal is
//! (rule 71: item-relative; `file:line` exists only when printed).
//!
//! Before this file a row was `(fn, class)`: build census run 5 over tempo's `main` walk
//! (port27/tempo-run/build-census-5.txt) had 3,499 fns under "check gap: RefusedImpl" and no
//! way to split them, because `Gap::RefusedImpl { impl_, place, why }` (the refused impl and
//! the reason) was dropped where the row was filed; `FnHead` (186) and `BodyConst` (84) lost
//! their item the same way. A row now keeps the refusal (`Front2Refusal::Check` holds the
//! `CheckFail` and so every `Gap`; `Lower` the `BodyError`), and its [`LeftOut::split`] names
//! the gap's own item through the one printer (row_path.rs [`gap_split`], over
//! [`ItemShown`](crate::front2::item_shown::ItemShown) and `shown_name`): no second printer.
//!
//! `Display` prints the class alone, the text the row's second field was before, so every
//! reader that printed `{class}` prints the same.

use super::*;

use crate::front2::row_path::gap_split;
use crate::front2::source::NamesSource;
use crate::front2::world_items::Gap;

/// Where a left-out fn's refusal is, item-relative (rule 71).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeftAt {
    /// A range of `item`'s own text: a diagnostic of the body, a const term with no value.
    In { item: ItemKey, range: tc::Range },
    /// A whole library item the refusal is about: a candidate impl whose header was refused
    /// or not read (`Gap::RefusedImpl`, `Gap::UnreadImpl`).
    Lib(LibKey),
    /// A whole checker item the refusal is about: a member fn whose signature was not made
    /// (`Gap::FnHead`).
    Item(ItemKey),
}

/// One fn a survey left out, beside its shown path in the survey's list.
#[derive(Debug)]
pub struct LeftOut {
    /// The cause's class, as the survey always filed it (`check gap: RefusedImpl`, `no row
    /// path: ..`, `constant: ..`): what a census groups by first.
    pub class: Box<str>,
    /// What splits the class into the causes behind it, empty where the class says all: for a
    /// gap, the item it is about by its path and why (`the impl <impl Tr for G<Missing>> (key
    /// ..): ..`), never the left-out fn's own name, a count or a `file:line`.
    pub split: Box<str>,
    /// The whole cause, by paths (reach.rs `check_failed`'s detail; a refusal's `Display`).
    pub whole: Box<str>,
    /// Where the refusal is, item-relative; `None` where the refusal names no place.
    pub at: Option<LeftAt>,
    /// The refusal itself: what a run would have stopped with.
    pub refusal: Front2Refusal,
}

impl LeftOut {
    /// A row whose class says all a census groups by (a refused static, constant, ADT row or
    /// object trait; a fn refused while its row was made: tabled.rs `left_out_or_stop`).
    pub(crate) fn named(class: Box<str>, refusal: Front2Refusal) -> LeftOut {
        let whole = format!("{refusal}").into_boxed_str();
        LeftOut { class, split: Box::default(), whole, at: None, refusal }
    }

    /// A fn the walk reached and could not check or lower (reach.rs `Reached::Failed`): its
    /// class and whole detail as the walk said them; the split and the place read from the
    /// typed refusal, its items printed by the one printer.
    pub(crate) fn reached<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, class: Box<str>, whole: Box<str>, refusal: Front2Refusal) -> LeftOut {
        let (split, at) = match &refusal {
            // The first gap is the one the class is named for (reach.rs `check_failed`).
            Front2Refusal::Check(CheckFail::Gap { gaps, .. }) => match gaps.first() {
                Some(g) => (gap_split(r, g).into_boxed_str(), gap_at(g)),
                None => (Box::default(), None),
            },
            Front2Refusal::Check(CheckFail::Diags { owner, diags, .. }) => (Box::default(), diags.first().map(|(_, range, _)| LeftAt::In { item: *owner, range: *range })),
            _ => (Box::default(), None),
        };
        LeftOut { class, split, whole, at, refusal }
    }

    /// The census's group: the class, then ` | ` and the split where there is one.
    pub fn group(&self) -> String {
        let mut out = String::with_capacity(self.class.len() + 3 + self.split.len());
        out.push_str(&self.class);
        if !self.split.is_empty() {
            out.push_str(" | ");
            out.push_str(&self.split);
        }
        out
    }
}

impl core::fmt::Display for LeftOut {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.class)
    }
}

/// The item a gap is about, item-relative.
fn gap_at(g: &Gap) -> Option<LeftAt> {
    match g {
        Gap::RefusedImpl { impl_, .. } => Some(LeftAt::Lib(*impl_)),
        Gap::UnreadImpl(impl_, _) => Some(LeftAt::Lib(*impl_)),
        Gap::FnHead(k, _) => Some(LeftAt::Item(*k)),
        Gap::BodyConst { owner, gap } => Some(LeftAt::In { item: *owner, range: tc::Range { start: gap.range.start, end: gap.range.end } }),
        _ => None,
    }
}
