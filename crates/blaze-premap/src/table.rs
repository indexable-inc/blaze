//! The image's symbol table: what the workspace link resolves against.
//!
//! Every symbol is a 32-byte ID ([`SymbolId`]): a function by its instance key, a static by
//! its item and instance, a name by its bytes, each in its own hash domain. IDs are BLAKE3
//! outputs, so their first 8 bytes index an open-addressed table directly, with no further
//! hashing: a power-of-two count of 64-byte slots (one cache line each), at most half full,
//! linear probing. A lookup is one cache miss and a 32-byte compare, usually; a miss ends at
//! the first empty slot. The table is read in place from the image's bytes ([`TableView`]),
//! checked once when the image is opened.
//!
//! A slot also says where the image keeps what its piece is beyond its address ([`Refs`]): a
//! static's definition, what a function built in, a function's debug name. So a build checks
//! that a piece of the image is its own in the cache line the lookup read.

use blaze_codegen::InstanceKey;
use blaze_ids::{ContentId, Hasher};
use blaze_items::ItemKey;
pub use blaze_link::premap::{Def, Kind, Strength};

use crate::PremapError;

pub const STATIC_SYMBOL_DOMAIN: &str = "blaze v2 premap symbol static 1";
pub const NAME_SYMBOL_DOMAIN: &str = "blaze v2 premap symbol name 1";

/// Bytes per slot: id 0..32, value 32..40, size 40..48, order 48..52, kind 52, strength 53,
/// reached directly 54 (0 or 1), a zero byte, content 56..60, debug name 60..64.
pub const SLOT: usize = 64;

/// A symbol's ID in the table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SymbolId(pub ContentId);

impl SymbolId {
    pub fn function(key: &InstanceKey) -> SymbolId {
        SymbolId(key.0)
    }

    pub fn of_static(item: &ItemKey, instance: &ContentId) -> SymbolId {
        SymbolId(Hasher::new(STATIC_SYMBOL_DOMAIN).id(&item.0).id(instance).finish())
    }

    pub fn name(name: &str) -> SymbolId {
        SymbolId(Hasher::new(NAME_SYMBOL_DOMAIN).part(name.as_bytes()).finish())
    }
}

/// Where the image keeps what a slot's piece is beyond its address, each an index from 1 into
/// one of the image's lists, 0 for none.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Refs {
    /// Into the image's contents: a static's definition ([`crate::key::static_definition`]),
    /// or a function's code and what it built in (`blaze_codegen::driver::digests::function_digest`).
    pub content: u32,
    /// Into the image's debug name digests: what a debugger calls a function
    /// (`blaze_codegen::driver::digests::debug_name_digest`).
    pub debug: u32,
}

/// A slot as read: its definition and its refs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub def: Def,
    pub refs: Refs,
}

/// Builds a table of `entries`: an error when an ID is twice in it, or is all zero (the
/// empty slot).
pub fn build(entries: &[(SymbolId, Def)]) -> Result<Vec<u8>, PremapError> {
    let slots = (entries.len() * 2).next_power_of_two().max(16);
    let mask = slots - 1;
    let mut out = vec![0u8; slots * SLOT]; // alloc: once per image made: its table
    for (id, def) in entries {
        if id.0.0 == [0; 32] {
            return Err(PremapError::of(format_args!("a symbol of the image has the empty ID")));
        }
        let mut at = start(&id.0) as usize & mask;
        loop {
            let slot = &mut out[at * SLOT..(at + 1) * SLOT];
            if slot[..32] == [0; 32] {
                slot[..32].copy_from_slice(&id.0.0);
                slot[32..40].copy_from_slice(&def.value.to_le_bytes());
                slot[40..48].copy_from_slice(&def.size.to_le_bytes());
                slot[48..52].copy_from_slice(&def.order.to_le_bytes());
                slot[52] = kind_tag(def.kind);
                slot[53] = strength_tag(def.strength);
                break;
            }
            if slot[..32] == id.0.0 {
                return Err(PremapError::of(format_args!("the symbol {:?} is in the image twice", id.0)));
            }
            at = (at + 1) & mask;
        }
    }
    Ok(out)
}

/// Sets [`Refs::content`] of `id`'s slot in a table [`build`] made: an error when it has none.
pub fn set_content(table: &mut [u8], id: &SymbolId, content: u32) -> Result<(), PremapError> {
    set_ref(table, id, 56, content)
}

/// Sets [`Refs::debug`] of `id`'s slot in a table [`build`] made: an error when it has none.
pub fn set_debug(table: &mut [u8], id: &SymbolId, debug: u32) -> Result<(), PremapError> {
    set_ref(table, id, 60, debug)
}

/// Marks `id`'s slot, a name the image defines weakly, as reached by the image's own code
/// other than through a bind ([`TableView::reached_directly`]): an error when it has no slot
/// or is defined strongly.
pub fn set_reached_directly(table: &mut [u8], id: &SymbolId) -> Result<(), PremapError> {
    let Some(slot) = TableView::trusted(table).find(&id.0) else {
        return Err(PremapError::of(format_args!("the image reaches {:?} directly and does not define it", id.0)));
    };
    if strength_of(table[slot * SLOT + 53]) != Some(Strength::Weak) {
        return Err(PremapError::of(format_args!("the image reaches {:?} directly and defines it strongly, not weakly", id.0)));
    }
    table[slot * SLOT + 54] = 1;
    Ok(())
}

fn set_ref(table: &mut [u8], id: &SymbolId, at: usize, value: u32) -> Result<(), PremapError> {
    let Some(slot) = TableView::trusted(table).find(&id.0) else {
        return Err(PremapError::of(format_args!("the image has no symbol {:?} to give its content", id.0)));
    };
    table[slot * SLOT + at..slot * SLOT + at + 4].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

/// A table read in place.
#[derive(Clone, Copy)]
pub struct TableView<'a> {
    bytes: &'a [u8],
    mask: usize,
}

impl<'a> TableView<'a> {
    /// [`TableView::parse_with`] of a table whose slots refer to nothing.
    pub fn parse(bytes: &'a [u8]) -> Result<TableView<'a>, PremapError> {
        TableView::parse_with(bytes, 0, 0)
    }

    /// Checks every slot once: a power-of-two count of whole slots, every kind and strength
    /// one there is, every ref within the `contents` and `debug` records the image has, every
    /// ID in the slot a probe from its start reaches without passing an empty one.
    pub fn parse_with(bytes: &'a [u8], contents: usize, debug: usize) -> Result<TableView<'a>, PremapError> {
        let bad = |what: std::fmt::Arguments<'_>| Err(PremapError::of(format_args!("the image's symbol table {what}")));
        if bytes.len() % SLOT != 0 {
            return bad(format_args!("is {} bytes, not whole slots", bytes.len()));
        }
        let slots = bytes.len() / SLOT;
        if !slots.is_power_of_two() || slots < 16 {
            return bad(format_args!("has {slots} slots, not a power of two of at least 16"));
        }
        let view = TableView { bytes, mask: slots - 1 };
        let mut used = 0usize;
        for at in 0..slots {
            let slot = &bytes[at * SLOT..(at + 1) * SLOT];
            if slot[..32] == [0; 32] {
                if slot[32..] != [0; 32] {
                    return bad(format_args!("has an empty slot {at} with bytes in it"));
                }
                continue;
            }
            used += 1;
            if kind_of(slot[52]).is_none() || strength_of(slot[53]).is_none() || slot[54] > 1 || slot[55] != 0 {
                return bad(format_args!("has slot {at} of no kind there is"));
            }
            if slot[54] == 1 && strength_of(slot[53]) != Some(Strength::Weak) {
                return bad(format_args!("has slot {at} reached directly and defined strongly"));
            }
            let refs = refs_of(slot);
            if refs.content as usize > contents || refs.debug as usize > debug {
                return bad(format_args!("has slot {at} referring past the image's contents or debug names"));
            }
            let id: [u8; 32] = slot[..32].try_into().expect("32 bytes");
            if view.find(&ContentId(id)) != Some(at) {
                return bad(format_args!("has slot {at} where no probe for its ID reaches"));
            }
        }
        if used * 2 > slots {
            return bad(format_args!("is more than half full ({used} of {slots})"));
        }
        Ok(view)
    }

    /// A table [`TableView::parse`] accepted before, with no checks.
    pub(crate) fn trusted(bytes: &'a [u8]) -> TableView<'a> {
        TableView { bytes, mask: bytes.len() / SLOT - 1 }
    }

    pub fn get(&self, id: &SymbolId) -> Option<Def> {
        let at = self.find(&id.0)?;
        Some(def_of(&self.bytes[at * SLOT..(at + 1) * SLOT]))
    }

    /// The slot `id` is in, for [`TableView::def_at`] with no probe and no digest.
    pub fn slot_of(&self, id: &SymbolId) -> Option<usize> {
        self.find(&id.0)
    }

    /// The definition in slot `at`, which [`TableView::slot_of`] gave.
    pub fn def_at(&self, at: usize) -> Def {
        def_of(&self.bytes[at * SLOT..(at + 1) * SLOT])
    }

    /// Whether `id` is a name the image defines weakly and its own code reaches other than
    /// through a bind, so no definition beside the image may win over it.
    pub fn reached_directly(&self, id: &SymbolId) -> bool {
        self.find(&id.0).is_some_and(|at| self.bytes[at * SLOT + 54] == 1)
    }

    /// [`TableView::get`] with the slot's refs, from the one line the lookup read.
    pub fn entry(&self, id: &SymbolId) -> Option<Entry> {
        let at = self.find(&id.0)?;
        let slot = &self.bytes[at * SLOT..(at + 1) * SLOT];
        Some(Entry { def: def_of(slot), refs: refs_of(slot) })
    }

    /// Every symbol: for tests and for the run's bindings, never on a lookup path.
    pub fn iter(&self) -> impl Iterator<Item = (SymbolId, Def)> + '_ {
        (0..=self.mask).filter_map(move |at| {
            let slot = &self.bytes[at * SLOT..(at + 1) * SLOT];
            if slot[..32] == [0; 32] {
                return None;
            }
            let id = SymbolId(ContentId(slot[..32].try_into().expect("32 bytes")));
            Some((id, self.get(&id).expect("in the table")))
        })
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn find(&self, id: &ContentId) -> Option<usize> {
        let mut at = start(id) as usize & self.mask;
        for _ in 0..=self.mask {
            let slot = &self.bytes[at * SLOT..at * SLOT + 32];
            if slot == id.0 {
                return Some(at);
            }
            if slot == [0; 32] {
                return None;
            }
            at = (at + 1) & self.mask;
        }
        None
    }
}

fn start(id: &ContentId) -> u64 {
    u64::from_le_bytes(id.0[..8].try_into().expect("8 bytes"))
}

fn def_of(slot: &[u8]) -> Def {
    Def {
        value: u64::from_le_bytes(slot[32..40].try_into().expect("8 bytes")),
        size: u64::from_le_bytes(slot[40..48].try_into().expect("8 bytes")),
        order: u32::from_le_bytes(slot[48..52].try_into().expect("4 bytes")),
        kind: kind_of(slot[52]).expect("checked by parse"),
        strength: strength_of(slot[53]).expect("checked by parse"),
    }
}

fn refs_of(slot: &[u8]) -> Refs {
    Refs {
        content: u32::from_le_bytes(slot[56..60].try_into().expect("4 bytes")),
        debug: u32::from_le_bytes(slot[60..64].try_into().expect("4 bytes")),
    }
}

fn kind_tag(kind: Kind) -> u8 {
    match kind {
        Kind::Function => 1,
        Kind::Data => 2,
        Kind::ThreadLocal => 3,
    }
}

fn kind_of(tag: u8) -> Option<Kind> {
    match tag {
        1 => Some(Kind::Function),
        2 => Some(Kind::Data),
        3 => Some(Kind::ThreadLocal),
        _ => None,
    }
}

fn strength_tag(strength: Strength) -> u8 {
    match strength {
        Strength::Strong => 1,
        Strength::Weak => 2,
    }
}

fn strength_of(tag: u8) -> Option<Strength> {
    match tag {
        1 => Some(Strength::Strong),
        2 => Some(Strength::Weak),
        _ => None,
    }
}
