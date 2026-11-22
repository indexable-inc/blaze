//! An item's chain: where it is in its text by names, the one identity of an item that no edit
//! elsewhere in the file moves (rule 71; drive.md, "End state: the whole pipeline on the
//! command", the gap row "One body identity" and piece 5).
//!
//! ONE HOME (rule 30). Three places walked this chain: blaze-scope's items record
//! (`items_record::grain::item_chain`, 32 bytes, which now calls [`item_chain`]), the command's
//! `work.rs` `item_chain` (the same walk without the unnamed entry's head, cut to 64 bits) and
//! its `body_name`. The command's two move to this one in the driver's lane:
//!
//! ```text
//! let chain: ContentId = blaze_file_items::item_chain(&outline, item, text);
//! ```
//!
//! and a body is then named by (its file, `chain`): the front end finds it with
//! `front2::project::ProjectWorld::body_by_chain(file, chain)`.
//!
//! The bytes hashed, for the item and then each entry it is written in, up to the text's root:
//! the kind's byte (`ItemKind::byte`), the name as written (its length as a little-endian
//! `u32`, then its bytes; empty for an entry with no name), the disambiguator (little-endian
//! `u32`) and, for an entry with no name (an impl, a `use`, a macro call), its head
//! ([`head_of`], 32 bytes), so two unnamed siblings never share a chain. The domain is the one
//! the items record was first written under, so a record written before the move still reads.

use blaze_ids::{ContentId, Hasher};
use blaze_surface::{Item, ItemIndex, ItemKind};

use crate::Outline;

/// The domain of an item's chain digest. Its text names blaze-scope because the items record's
/// grain rows were first written under it: the bytes did not change when the function moved.
pub const CHAIN_DOMAIN: &str = "blaze-scope item chain 1";

/// `item`'s chain in `outline` of `text` (see the module docs for the bytes). A walk up the
/// parents, no recursion; nothing of the machine, a path or a position is hashed.
pub fn item_chain(outline: &Outline<'_>, item: &Item, text: &str) -> ContentId {
    chain_from(outline, Some(item), text)
}

/// The chain of the entry at `at`: [`item_chain`] by index.
pub fn chain_at(outline: &Outline<'_>, at: ItemIndex, text: &str) -> ContentId {
    chain_from(outline, Some(outline.item(at)), text)
}

fn chain_from<'o>(outline: &'o Outline<'_>, mut at: Option<&'o Item>, text: &str) -> ContentId {
    let mut h = Hasher::new(CHAIN_DOMAIN);
    while let Some(entry) = at {
        let name = entry.name.map_or("", |range| entry.text(text, range));
        h.update(&[entry.kind().byte()]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&entry.disambiguator.to_le_bytes());
        if entry.name.is_none() {
            h.update(&head_of(entry).0);
        }
        at = entry.parent.map(|up| outline.item(up));
    }
    h.finish()
}

/// The piece of `item`'s bytes its facts (kind, name, module, key, constructor, impl header)
/// are read from: its signature's digest, or its whole digest where the body is part of those
/// facts (a struct's, enum's or union's fields say whether it is its own constructor; a macro
/// definition's or an item-position macro call's tokens make names).
pub fn head_of(item: &Item) -> ContentId {
    match item.kind() {
        ItemKind::Struct | ItemKind::Enum | ItemKind::Union | ItemKind::Variant | ItemKind::MacroRules | ItemKind::Macro2 | ItemKind::MacroCall | ItemKind::Error | ItemKind::Unhandled => item.hash,
        ItemKind::Function
        | ItemKind::Trait
        | ItemKind::Impl
        | ItemKind::Const
        | ItemKind::Static
        | ItemKind::TypeAlias
        | ItemKind::Module
        | ItemKind::Use
        | ItemKind::ExternCrate
        | ItemKind::Field
        | ItemKind::TraitAlias
        | ItemKind::ExternBlock => item.signature_hash,
    }
}
