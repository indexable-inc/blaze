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

/// The source key of the written local declared at byte `declaration` of `top`'s text (the
/// lowering's start of it), and the byte its site counts from: [`first_token`] of it, the start
/// [`find`] and the cold reading ([`in_source`]) give the same declaration.
pub(super) fn key_at<'n, S: NamesSource<'n>>(names: S, krate: usize, top: Anchor<'n>, declaration: u32, alloc: Alloc<'_>) -> Result<(LibKey, u32), PositionGap> {
    let text = top.text(names).ok_or(PositionGap::NoItem)?;
    let key = names.crate_key(krate).ok_or(PositionGap::NoItem)?;
    let edition = names.edition_of(krate).ok_or(PositionGap::NoItem)?;
    let first = first_token(text, declaration).ok_or(PositionGap::PastText)?;
    find(key, top.lib(), text, edition, Find::Declaration(first), alloc)?.ok_or(PositionGap::NoItem)
}

/// THE WRITTEN-LOCAL START RULE: the first byte of the first token at or after byte `byte` of
/// `text`, past whitespace and comments, doc comments among them. A local item as the lowering
/// ranges it starts at its first outer attribute, and a doc comment is one (the Reference,
/// "Comments", "Doc comments": `///` is the attribute `#[doc = ".."]`); the text scan [`find`]
/// keys declarations by (blaze-file-items nested.rs `scan`) reads comments as whitespace, so
/// its declaration starts at the first token after them. curve25519-dalek 4.1.3's `fn m`, local
/// to `<&FieldElement51 as Mul>::mul` (field.rs:116, two `///` lines, then
/// `#[inline(always)]`), was ranged from its comment and matched no declaration of the scan, so
/// its overflow check had no `Location` (tempo's tests, t1530: 42 not run). The site of a
/// written local counts from this byte, warm ([`super::caller_site`], [`super::source_position`])
/// and cold ([`in_source`]) alike. Whitespace is Rust's (`Pattern_White_Space`, the Reference,
/// "Whitespace"); a block comment nests ("Comments"). `None`: `byte` is past the text or inside
/// a char, a block comment is not closed, or no token follows.
pub(super) fn first_token(text: &str, byte: u32) -> Option<u32> {
    let mut at = byte as usize;
    // Each turn moves past at least one char; the text bounds the turns.
    for _ in 0..=text.len() {
        let rest = text.get(at..)?;
        let next = rest.as_bytes().get(1).copied();
        match rest.chars().next()? {
            c if matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{85}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}') => at += c.len_utf8(),
            '/' if next == Some(b'/') => at += rest.find('\n').unwrap_or(rest.len()),
            '/' if next == Some(b'*') => {
                let bytes = rest.as_bytes();
                let mut depth = 0u32;
                let mut i = 0usize;
                loop {
                    match (bytes.get(i).copied(), bytes.get(i + 1).copied()) {
                        (Some(b'/'), Some(b'*')) => {
                            depth += 1;
                            i += 2;
                        }
                        (Some(b'*'), Some(b'/')) => {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        }
                        (Some(_), Some(_)) => i += 1,
                        _ => return None,
                    }
                }
                at += i;
            }
            _ => return u32::try_from(at).ok(),
        }
    }
    None
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

/// The item or member whose body's blocks the text `fragment` (outlined as `outline`) is the
/// module text of: the one key the scope solver gives that text, `(body, "blocks", "", 0)`
/// (blaze-scope resolve.rs `make_blocks`), found among the crate's bodies that hold items.
/// `Ok(None)`: the text is a macro call's expansion. A real macro can also declare a module
/// with the block modules' spelling, so the spelling only rules a text out; the fragment key
/// rules it in. Two bodies under one key: refused (`NoItem`).
fn block_body_of<'n, S: PositionSource<'n>>(
    names: S, krate: usize, outline: &blaze_file_items::Outline<'_>, text: &str, fragment: LibKey,
) -> Result<Option<LibKey>, PositionGap> {
    let is_block = outline.children(None).any(|(_, item)| item.kind() == ItemKind::Module
        && item.name.is_some_and(|r| blaze_scope::blocks::is_block_module(item.text(text, r))));
    if !is_block {
        return Ok(None);
    }
    let crate_key = names.crate_key(krate).ok_or(PositionGap::NoItem)?;
    let mut parent = None;
    names.enclosing_keys(krate, |lib| {
        if blaze_scope::task::key_of(crate_key, lib, "blocks", "", 0) == fragment {
            if parent.replace(lib).is_some() {
                return Err(PositionGap::NoItem);
            }
        }
        Ok(())
    })?;
    Ok(parent)
}

/// A macro call written in a block of a body, placed in that body's own text. The names
/// library writes a body's blocks as module text, a text of its own that is no macro's
/// expansion and keeps no call origins (resolve.rs `make_blocks`), so a call written there
/// (tempo's `#[derive(serde::Serialize)] struct BlockRef` in `fn serialize`; rand's
/// `const _: () = unsafe { impl_fill!(..) };`) has its site in the text the block's
/// rendering copied it from. `text` is the fragment `fragment`'s text and `at` gives a byte of
/// it (the call's site, `invoked_at`, read only for a body's blocks); the answer is the byte of
/// the body's text it was copied from, the body's text rendered again today (`module_text`,
/// the one renderer). `Ok(None)`: `fragment` is a macro call's expansion, not a body's blocks.
/// Refused (`NoItem`): today's rendering is not the text the call was counted in, or the byte
/// is in no copied slice.
pub(super) fn block_call_site<'n, S: PositionSource<'n>>(
    names: S, krate: usize, fragment: LibKey, text: &str, at: impl FnOnce() -> Result<u32, PositionGap>, alloc: Alloc<'_>,
) -> Result<Option<(Anchor<'n>, u32)>, PositionGap> {
    let edition = names.edition_of(krate).ok_or(PositionGap::NoFile)?;
    let outline = blaze_file_items::outline(text, edition, alloc);
    let Some(lib) = block_body_of(names, krate, &outline, text, fragment)? else { return Ok(None) };
    let body = Anchor::from_lib(names, lib).ok_or(PositionGap::NoItem)?;
    let written = body.text(names).ok_or(PositionGap::NoItem)?;
    Ok(Some((body, copied_from(written, text, at()?, alloc)?)))
}

/// The byte of `body` that byte `at` of its blocks' module text `rendered` was copied from:
/// `body` rendered again (`blaze_scope::blocks::module_text`) must be `rendered`, else the
/// byte would be counted in another text (refused, `NoItem`, as a byte in no copied slice).
fn copied_from(body: &str, rendered: &str, at: u32, alloc: Alloc<'_>) -> Result<u32, PositionGap> {
    let nested = blaze_file_items::nested::scan(body, alloc);
    if nested.end != NestedEnd::Whole {
        return Err(PositionGap::NoItem);
    }
    let mut copies = AVec::new_in(alloc);
    let today = blaze_scope::blocks::module_text(body, &nested, &mut |c| copies.push(c), alloc);
    if today.as_str() != rendered {
        return Err(PositionGap::NoItem);
    }
    for copy in copies {
        let end = copy.to.checked_add(copy.len).ok_or(PositionGap::PastText)?;
        if copy.to <= at && at < end {
            return copy.from.checked_add(at - copy.to).ok_or(PositionGap::PastText);
        }
    }
    Err(PositionGap::NoItem)
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
    let Some(lib) = block_body_of(names, krate, &outline, original, call)? else { return Ok(None) };
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
    // The one item of this chain in the block module's text as rendered now: an outline of
    // this small text alone (not a lookup in a record; the items record no longer finds an item
    // by chain, lane A4). Two items of one chain is no answer.
    let outline = blaze_file_items::outline(&rendered, edition, alloc);
    let mut current = None;
    for item in outline.items() {
        if blaze_file_items::item_chain(&outline, item, &rendered) == chain {
            if current.is_some() {
                return Err(PositionGap::NoItem);
            }
            current = Some(item.range);
        }
    }
    let range = current.ok_or(PositionGap::NoItem)?;
    for copy in copies {
        let end = copy.to.checked_add(copy.len).ok_or(PositionGap::PastText)?;
        if !(copy.to <= range.start && range.start < end) {
            continue;
        }
        // A `(..)` or `[..]` call is copied and then given the `;` an item-position call ends
        // with (blaze-scope expanded_blocks.rs `render`, `Event::Call`): rand's tail call
        // `const _: () = unsafe { impl_fill!(..) };` is written `impl_fill!(..);`. That `;` is
        // the renderer's, no byte of the body: the call's written range ends with its copy.
        // Before (tempo t1855d), the item's range was never inside one copy, and every
        // expression call in a block had no place (`NoItem`).
        let last = match rendered.get(end as usize..range.end as usize) {
            _ if range.end <= end => range.end,
            Some(";") => end,
            _ => return Err(PositionGap::NoItem),
        };
        let start = copy.from.checked_add(range.start - copy.to).ok_or(PositionGap::PastText)?;
        let end = start.checked_add(last - range.start).ok_or(PositionGap::PastText)?;
        return Ok(Range::new(start, end));
    }
    Err(PositionGap::NoItem)
}

#[cfg(test)]
mod tests {
    use super::first_token;

    /// curve25519-dalek's shape (two `///` lines, then `#[inline(always)]`): the declaration
    /// starts at the `#`, where the text scan starts it; a nested block comment and a plain
    /// `//` are skipped too. The control: a byte already at a token is that byte. An unclosed
    /// block comment, or no token after the comments, is no answer.
    #[test]
    fn a_declaration_starts_past_its_doc_comments() {
        let text = "    /// Helper.\n    /// Bits.\n    #[inline(always)]\n    fn m() {}";
        assert_eq!(first_token(text, 4), text.find("#[inline").map(|b| b as u32));
        let nested = "/* a /* b */ c */ // d\n\u{2028}fn m() {}";
        assert_eq!(first_token(nested, 0), nested.find("fn").map(|b| b as u32));
        let at = text.find("fn m").unwrap() as u32;
        assert_eq!(first_token(text, at), Some(at));
        assert_eq!(first_token("/* open /* */ fn", 0), None);
        assert_eq!(first_token("/// only a comment", 0), None);
        std::println!("a local's declaration starts past its doc comments, at its first token");
    }
}
