//! An item declared inside a body has no independent names-record row. Identify
//! its source by its lexical chain of declarations under the nearest item or
//! member that does have a row. A macro preceding the declaration can change
//! the checker's ordinal by contributing more declarations, but cannot change
//! the ordinal of the declarations actually written in the enclosing text.

use super::{Anchor, LibKey, NamesSource, PositionGap, PositionSource, Start, checker_of, position_at};
use blaze_file_items::nested::{NestedEnd, NestedKind};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Edition, ItemKind, Range};
use blaze_typeck::ItemKey;

#[derive(Clone, Copy)]
enum Find {
    Declaration(u32),
    Key(ItemKey),
}

/// Walk only declarations nested in this one body. For a known source byte
/// descend along its containing declarations only. For a key keep a second
/// match as a refusal, as for a library key whose first 16 bytes collide.
fn find(
    krate: CrateItemKey, top: LibKey, text: &str, edition: Edition, seek: Find, alloc: Alloc<'_>,
) -> Result<Option<(LibKey, u32)>, PositionGap> {
    let mut bodies = AVec::new_in(alloc);
    bodies.push((top, 0u32, text));
    let mut found = None;
    while let Some((parent, base, body)) = bodies.pop() {
        let nested = blaze_file_items::nested::scan(body, alloc);
        if nested.end != NestedEnd::Whole {
            return Err(PositionGap::NoItem);
        }
        let mut ranks = blaze_hash::HashMap::new_in(alloc);
        for entry in nested.items.iter() {
            let tag = match entry.kind {
                NestedKind::Fn => "written local fn",
                NestedKind::Const => "written local const",
                NestedKind::Static => "written local static",
                _ => continue,
            };
            let at = base.checked_add(entry.range.start).ok_or(PositionGap::PastText)?;
            if matches!(seek, Find::Declaration(byte) if byte < at) {
                break;
            }
            let part = body.get(entry.range.start as usize..entry.range.end as usize).ok_or(PositionGap::PastText)?;
            let declaration = blaze_file_items::outline_item(part, edition, alloc);
            let item = declaration.items().first().ok_or(PositionGap::NoItem)?;
            let name = item.name.map(|r| item.text(part, r)).unwrap_or("_");
            let ordinal = ranks.entry((tag, name)).or_insert(0u32);
            let mut key = blaze_scope::task::key_of(krate, parent, tag, name, *ordinal);
            key.0.0[8] |= LibKey::INNER;
            *ordinal = ordinal.checked_add(1).ok_or(PositionGap::NoItem)?;
            match seek {
                Find::Declaration(byte) if byte == at => return Ok(Some((key, at))),
                Find::Declaration(byte) if byte >= base.checked_add(entry.range.end).ok_or(PositionGap::PastText)? => continue,
                Find::Key(id) if checker_of(key) == id => {
                    if found.replace((key, at)).is_some() {
                        return Err(PositionGap::NoItem);
                    }
                }
                _ => {}
            }
            // A nested body is a strict subrange. The total work list cannot
            // exceed the number of declarations in the top item's text.
            if part.len() >= body.len() {
                return Err(PositionGap::PastText);
            }
            bodies.push((key, at, part));
        }
    }
    Ok(found)
}

pub(super) fn key_at<'n, S: NamesSource<'n>>(names: S, krate: usize, top: Anchor<'n>, declaration: u32, alloc: Alloc<'_>) -> Result<LibKey, PositionGap> {
    let text = top.text(names).ok_or(PositionGap::NoItem)?;
    let key = names.crate_key(krate).ok_or(PositionGap::NoItem)?;
    let edition = names.edition_of(krate).ok_or(PositionGap::NoItem)?;
    find(key, top.lib(), text, edition, Find::Declaration(declaration), alloc)?.map(|(key, _)| key).ok_or(PositionGap::NoItem)
}

/// A written local's key can arrive before its enclosing body is checked.
pub(super) fn in_source<'n, S: PositionSource<'n>>(
    names: S, krate: usize, wanted: ItemKey, site: u32, alloc: Alloc<'_>,
) -> Result<(u32, u32), PositionGap> {
    let key = names.crate_key(krate).ok_or(PositionGap::NoItem)?;
    let edition = names.edition_of(krate).ok_or(PositionGap::NoItem)?;
    let mut found = None;
    names.enclosing_keys(krate, |lib| {
        let anchor = Anchor::from_lib(names, lib).ok_or(PositionGap::NoItem)?;
        let text = anchor.text(names).ok_or(PositionGap::NoItem)?;
        if let Some((_, start)) = find(key, lib, text, edition, Find::Key(wanted), alloc)? {
            if found.replace((anchor, start)).is_some() {
                return Err(PositionGap::NoItem);
            }
        }
        Ok(())
    })?;
    let (top, old_start) = found.ok_or(PositionGap::NoItem)?;
    let place = top.placement(names, alloc)?;
    let start = match place.2 {
        Start::Copied(_, body) => find(key, top.lib(), body, edition, Find::Key(wanted), alloc)?.ok_or(PositionGap::NoItem)?.1,
        _ => old_start,
    };
    position_at(names, place, start.checked_add(site).ok_or(PositionGap::PastText)?, alloc)
}

/// The scope solver renders item statements into block modules and keys that
/// fragment from the enclosing body's library key (resolve.rs `make_blocks`).
/// A copied method still denotes the method written in the function. Locate its
/// declaration in today's rendering by outline chain, then invert the renderer's
/// copy range. Whitespace inside the enclosing body can change either offset.
pub(super) fn copied_declaration<'n, S: PositionSource<'n>>(
    names: S, krate: usize, anchor: Anchor<'n>, call: LibKey, alloc: Alloc<'_>,
) -> Result<Option<(Anchor<'n>, ContentId)>, PositionGap> {
    let original = names.expansion_text(krate, call).ok_or(PositionGap::NoFile)?;
    let edition = names.edition_of(krate).ok_or(PositionGap::NoFile)?;
    let outline = blaze_file_items::outline(original, edition, alloc);
    let is_block = outline.children(None).any(|(_, item)| item.kind() == ItemKind::Module
        && item.name.is_some_and(|r| blaze_scope::blocks::is_block_module(item.text(original, r))));
    if !is_block {
        return Ok(None);
    }
    let crate_key = names.crate_key(krate).ok_or(PositionGap::NoItem)?;
    let mut parent = None;
    names.enclosing_keys(krate, |lib| {
        if blaze_scope::task::key_of(crate_key, lib, "blocks", "", 0) == call {
            if parent.replace(lib).is_some() {
                return Err(PositionGap::NoItem);
            }
        }
        Ok(())
    })?;
    // A real macro can also declare a module with this spelling. Only the
    // fragment key establishes that the scope solver made this expansion.
    let Some(lib) = parent else { return Ok(None) };
    let parent = Anchor::from_lib(names, lib).ok_or(PositionGap::NoItem)?;
    let index = match anchor {
        Anchor::Item(lib) => names.place_of(lib).ok_or(PositionGap::NoItem)?.item,
        Anchor::Member { row, .. } => row.item,
    };
    let entry = outline.items().get(index.0 as usize).ok_or(PositionGap::NoItem)?;
    Ok(Some((parent, blaze_file_items::item_chain(&outline, entry, original))))
}

/// The original source range of the declaration with this outline chain in the
/// block module rendered from today's enclosing body.
pub(super) fn written_range(body: &str, chain: ContentId, edition: Edition, alloc: Alloc<'_>) -> Result<Range, PositionGap> {
    let nested = blaze_file_items::nested::scan(body, alloc);
    if nested.end != NestedEnd::Whole {
        return Err(PositionGap::NoItem);
    }
    let mut copies = AVec::new_in(alloc);
    let rendered = blaze_scope::blocks::module_text(body, &nested, &mut |c| copies.push(c), alloc);
    let current = blaze_scope::items_record::grain::outline_today(&rendered, chain, edition, alloc).ok_or(PositionGap::NoItem)?;
    let range = current.item.range;
    for copy in copies {
        let end = copy.to.checked_add(copy.len).ok_or(PositionGap::PastText)?;
        if copy.to <= range.start && range.end <= end {
            let start = copy.from.checked_add(range.start - copy.to).ok_or(PositionGap::PastText)?;
            let end = start.checked_add(range.end - range.start).ok_or(PositionGap::PastText)?;
            return Ok(Range::new(start, end));
        }
    }
    Err(PositionGap::NoItem)
}
