//! A body's names as written: the checker asks `resolve_path` by name hashes
//! (`Name::of(text)`), and the names library resolves written text, so each name the body's
//! lowering interned is read back from its range in the lowering's own text (the item's text
//! or a macro's expansion, [`View::text_at`]). A hash the table does not hold is the
//! [`Unspelled`] refusal, never a guess.

use std::boxed::Box;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::View;
use blaze_lower::form::NameIdx;
use blaze_own::Alloc;
use blaze_typeck::Name;

use super::source::NamesSource;
use super::types::Why;

/// The path heads blaze-lower writes as synthetic segments (blaze-lower's `push_segments`).
const FIXED: [(Name, &str); 6] = [
    (Name::of("self"), "self"),
    (Name::of("super"), "super"),
    (Name::of("crate"), "crate"),
    (Name::of("Self"), "Self"),
    (Name::of("$crate"), "$crate"),
    (Name::of("{{root}}"), "{{root}}"),
];

/// Inline capacity for small/medium bodies so `Spelling::of` and `Spelling::fixed` make zero heap
/// allocations on the common case.
const INLINE_CAP: usize = 24;

/// A name hash the body's lowering holds no text for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unspelled(pub Name);

enum Entries<'b> {
    Inline {
        entries: [(Name, &'b str); INLINE_CAP],
        len: usize,
    },
    Map(HashMap<Name, &'b str, Alloc<'static>>),
}

/// Each name of one body by its hash.
pub struct Spelling<'b> {
    entries: Entries<'b>,
}

impl<'b> Spelling<'b> {
    /// The names `view`'s lowering interned with a range (a synthetic name has an empty one
    /// and is one of [`FIXED`] or no path segment).
    pub fn of(view: &View<'b>) -> Result<Spelling<'b>, Why> {
        Self::of_in(view, Alloc::global())
    }

    /// Like [`Spelling::of`], allocating any overflow table in `alloc`.
    pub fn of_in(view: &View<'b>, alloc: Alloc<'static>) -> Result<Spelling<'b>, Why> {
        if view.names.len() <= INLINE_CAP {
            let mut entries = [(Name(0), ""); INLINE_CAP];
            let mut len = 0usize;
            for i in 0..view.names.len() {
                let Ok(idx) = u32::try_from(i) else { break };
                let n = NameIdx(idx);
                let r = view.name_range(n)?;
                let text = view.text_at(r)?;
                if text.is_empty() {
                    continue;
                }
                let text = text.strip_prefix("r#").unwrap_or(text);
                if let Some(slot) = entries.get_mut(len) {
                    *slot = (Name(view.name(n)?), text);
                    len += 1;
                }
            }
            return Ok(Spelling { entries: Entries::Inline { entries, len } });
        }
        let mut by_name = HashMap::with_capacity_in(view.names.len(), alloc);
        for i in 0..view.names.len() {
            let Ok(idx) = u32::try_from(i) else { break };
            let n = NameIdx(idx);
            let r = view.name_range(n)?;
            let text = view.text_at(r)?;
            if text.is_empty() {
                continue;
            }
            let text = text.strip_prefix("r#").unwrap_or(text);
            by_name.insert(Name(view.name(n)?), text);
        }
        Ok(Spelling { entries: Entries::Map(by_name) })
    }

    /// The spelling of no body: only the synthetic heads ([`FIXED`]). For a world that asks
    /// questions over ground types and resolves no written path (front2/const_project.rs: a
    /// projection normalized for a constant's layout); any name it is asked is [`Unspelled`].
    pub fn fixed() -> Spelling<'static> {
        Spelling { entries: Entries::Inline { entries: [(Name(0), ""); INLINE_CAP], len: 0 } }
    }

    pub fn get(&self, n: Name) -> Result<&'b str, Unspelled> {
        let found = match &self.entries {
            Entries::Inline { entries, len } => entries.get(..*len).unwrap_or(&[]).iter().rev().find(|(k, _)| *k == n).map(|&(_, s)| s),
            Entries::Map(by_name) => by_name.get(&n).copied(),
        };
        if let Some(s) = found {
            return Ok(s);
        }
        FIXED.iter().find(|(k, _)| *k == n).map(|&(_, s)| s).ok_or(Unspelled(n))
    }
}

/// Why a name could not be spelled from an item's own text ([`spell_in_item`]).
#[derive(Debug)]
pub enum ItemUnspelled {
    /// The names library holds no text for the item.
    NoText(LibKey),
    /// Its head did not lower (heads.rs `lower`).
    Head(super::heads::HeadError),
    /// Its lowering's names did not read back.
    Names(Why),
    /// Its lowering interns no such name.
    Absent(LibKey, Unspelled),
}

/// The spelling of `n` in the head of `item` as its own text writes it: a name a finding
/// carries that the body never wrote (E0063's unwritten field, named by the ADT's head). The
/// item's head is lowered again from its text and read as a body's is ([`Spelling::of`]).
pub fn spell_in_item<'n, S: NamesSource<'n>>(names: S, item: LibKey, n: Name) -> Result<Box<str>, ItemUnspelled> {
    let text = names.text_of(item).ok_or(ItemUnspelled::NoText(item))?.text;
    super::heads::with_lowered(names, item, text, |view| {
        let spell = Spelling::of(view).map_err(ItemUnspelled::Names)?;
        spell.get(n).map(Box::from).map_err(|u| ItemUnspelled::Absent(item, u))
    })
    .map_err(ItemUnspelled::Head)?
}
