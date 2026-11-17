//! A `Signature`: one head the checker asks for, as a drive task (D114, drive.md "Check times:
//! the end state in numbers" rule 4, and "D114 rulings / Signatures").
//!
//! THE UNIT is one `heads::Want` of front2 (blaze-ladder front2/project/head_digest.rs): a fn,
//! `const` or `static`; a struct, enum or union with its variants and constructors; a trait with
//! its members' fn signatures; an impl's header with its member list; one fn of an impl; a type
//! alias or trait alias. A trait's member has no `Signature` of its own (its trait's covers it);
//! an impl's fn has.
//!
//! ITS NAME ([`signature_name`]): the file's subject (the file's real path in the checkout, as
//! the items records list it after `/0/`, so the body that reads it and the file job that makes
//! it name it alike) and the entry's chain up to the file, as work.rs `body_name` walks it.
//!
//! ITS SOURCE ([`signature_source`]), its key's text: the entry's head bytes (a fn's, `const`'s or
//! `static`'s signature; a struct's, enum's, union's or alias's whole text, whose fields are its
//! head), for a trait or impl each member's (kind, name, disambiguator) and, for a trait's fn and
//! any non-fn member, that member's bytes; each enclosing impl's or trait's signature; and the
//! file's `Surface` (what the head's paths resolve through: its `use`s and declarations). A body
//! edit leaves it equal; any edit to the head moves it.
//!
//! ITS OUT ([`signature_out`]): the digest of the head records the want makes
//! (`ProjectWorld::want_digest`) with [`unencoded`], the head's tokens without whitespace or
//! comments, for what the head record form leaves out by name (a fn's and an ADT's lifetimes, an
//! impl's const parameters, a trait's or impl's non-fn members). A reformat or a comment leaves
//! the out equal; a type edit moves it, and with it each body that read it (body_types.rs).

use blaze_drive::{Kind, Name};
use blaze_file_items::Outline;
use blaze_ids::{ContentId, Hasher};
use blaze_surface::{Edition, Item, ItemIndex, ItemKind};

/// The domain of a `Signature`'s subject ([`signature_name`]).
const NAME_DOMAIN: &str = "blaze-cli signature name 1";
/// The domain of a `Signature` part's source ([`signature_source`]).
const SOURCE_DOMAIN: &str = "blaze-cli signature source 1";
/// The domain of a head's tokens ([`unencoded`]).
const TOKENS_DOMAIN: &str = "blaze-cli signature tokens 1";
/// The domain of a `Signature`'s out ([`signature_out`]).
const OUT_DOMAIN: &str = "blaze-cli signature out 1";
/// The want digest of an entry front2 makes no head for (a `const _`, an item a macro rewrote):
/// its out is then its tokens alone, and no body reads it (body_types.rs reads by want).
pub(crate) const NO_WANT: ContentId = ContentId([0; 32]);

/// Whether `kind` is a head of its own when written outside any body and any trait or impl.
const fn own_head(kind: ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Function
            | ItemKind::Const
            | ItemKind::Static
            | ItemKind::Struct
            | ItemKind::Enum
            | ItemKind::Union
            | ItemKind::Trait
            | ItemKind::TraitAlias
            | ItemKind::Impl
            | ItemKind::TypeAlias
    )
}

/// Whether every entry `item` is written in, up to the file, is a module: it is in no body, no
/// trait, no impl, no extern block.
fn in_modules_only(outline: &Outline<'_>, item: &Item) -> bool {
    let mut parent = item.parent;
    while let Some(at) = parent {
        let up = outline.item(at);
        if up.kind() != ItemKind::Module {
            return false;
        }
        parent = up.parent;
    }
    true
}

/// Whether `item` of `outline` has a `Signature` task: a head of its own in modules only, or a
/// fn of an impl that is. The one predicate: the file job makes a part for exactly these
/// (work.rs `outline_file`), and a body names a head only when its entry passes it.
pub(crate) fn is_signature_entry(outline: &Outline<'_>, item: &Item) -> bool {
    if own_head(item.kind()) {
        return in_modules_only(outline, item);
    }
    let Some(parent) = item.parent else { return false };
    let up = outline.item(parent);
    item.kind() == ItemKind::Function && up.kind() == ItemKind::Impl && in_modules_only(outline, up)
}

/// The subject of a file's `Signature`s: its real path in the checkout.
pub(crate) fn file_subject(real: &str) -> ContentId {
    Name::file(real.as_bytes()).subject
}

/// The `Signature` of `item` (an entry passing [`is_signature_entry`]) in the file of subject
/// `file` (its [`file_subject`]), over its `outline` of `text`.
pub(crate) fn signature_name(file: ContentId, outline: &Outline<'_>, item: &Item, text: &str) -> Name {
    let mut h = Hasher::new(NAME_DOMAIN);
    h.update(&file.0);
    let mut at = Some(item);
    while let Some(entry) = at {
        let name = entry.name.map_or("", |range| entry.text(text, range));
        h.update(&[entry.kind() as u8]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&entry.disambiguator.to_le_bytes());
        at = entry.parent.map(|up| outline.item(up));
    }
    Name::new(Kind::Signature, h.finish())
}

/// Whether an entry's fields or variants are its head: its whole text is.
const fn whole_is_head(kind: ItemKind) -> bool {
    matches!(kind, ItemKind::Struct | ItemKind::Enum | ItemKind::Union | ItemKind::TypeAlias | ItemKind::TraitAlias)
}

/// The digest of an entry's head bytes: its whole text where its fields are its head, else its
/// signature (blaze-file-items' `signature_hash`: attributes, visibility, name, generics, up to
/// its body, or up to a `const`'s or `static`'s `=`).
fn head_bytes(item: &Item) -> ContentId {
    if whole_is_head(item.kind()) { item.hash } else { item.signature_hash }
}

/// The source of the `Signature` part of entry `at` of `outline` (see the module docs);
/// `surface` is its file's (`Outline::surface`).
pub(crate) fn signature_source(outline: &Outline<'_>, at: ItemIndex, text: &str, surface: ContentId) -> ContentId {
    let item = outline.item(at);
    let mut h = Hasher::new(SOURCE_DOMAIN);
    h.update(&surface.0).update(&[item.kind() as u8]).update(&head_bytes(item).0);
    if matches!(item.kind(), ItemKind::Trait | ItemKind::Impl) {
        let trait_ = item.kind() == ItemKind::Trait;
        let mut count = 0u32;
        for (_, member) in outline.children(Some(at)) {
            count += 1;
            let name = member.name.map_or("", |range| member.text(text, range));
            h.update(&[member.kind() as u8]).update(&(name.len() as u32).to_le_bytes()).update(name.as_bytes()).update(&member.disambiguator.to_le_bytes());
            // An impl's fn is its own `Signature`; a trait's fn and every other member are part
            // of this one. A trait's fn's default body is not a head: its signature is.
            if member.kind() == ItemKind::Function {
                if trait_ {
                    h.update(&member.signature_hash.0);
                }
            } else {
                h.update(&member.hash.0);
            }
        }
        h.update(&count.to_le_bytes());
    }
    // Each enclosing impl's or trait's signature: an impl's generics and self type are its fn's.
    let mut parent = item.parent;
    while let Some(at) = parent {
        let up = outline.item(at);
        if matches!(up.kind(), ItemKind::Impl | ItemKind::Trait) {
            h.update(&[up.kind() as u8]).update(&up.signature_hash.0);
        }
        parent = up.parent;
    }
    h.finish()
}

/// The tokens of `piece` in `edition`, whitespace and comments dropped, each after its length.
fn tokens(h: &mut Hasher, piece: &str, edition: Edition) {
    let pe = crate::work::parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, piece);
    for i in 0..lexed.len() {
        if lexed.kind(i).is_trivia() {
            continue;
        }
        let range = lexed.text_range(i);
        let token = &piece[range.start..range.end];
        h.update(&(token.len() as u32).to_le_bytes()).update(token.as_bytes());
    }
}

/// What the head record form leaves out of `item`'s head (see the module docs), as the head's
/// tokens without whitespace or comments: its signature's (a struct's, enum's, union's or
/// alias's whole text's), and for a trait each member's signature and each non-fn member's
/// whole text, for an impl each non-fn member's whole text. It holds the head's lifetimes and
/// an impl's const parameters, which head records do not; a reformat leaves it equal.
pub(crate) fn unencoded(outline: &Outline<'_>, at: ItemIndex, text: &str, edition: Edition) -> ContentId {
    let item = outline.item(at);
    let mut h = Hasher::new(TOKENS_DOMAIN);
    let head = if whole_is_head(item.kind()) { item.range.of(text) } else { item.text(text, item.signature) };
    tokens(&mut h, head, edition);
    if matches!(item.kind(), ItemKind::Trait | ItemKind::Impl) {
        let trait_ = item.kind() == ItemKind::Trait;
        for (_, member) in outline.children(Some(at)) {
            let piece = if member.kind() != ItemKind::Function {
                member.range.of(text)
            } else if trait_ {
                member.text(text, member.signature)
            } else {
                continue;
            };
            h.update(&[0xff, member.kind() as u8]);
            tokens(&mut h, piece, edition);
        }
    }
    h.finish()
}

/// A `Signature`'s out: the want's head records' digest (`want_digest`, or [`NO_WANT`]) and
/// [`unencoded`].
pub(crate) fn signature_out(want_digest: ContentId, unencoded: ContentId) -> ContentId {
    let mut h = Hasher::new(OUT_DOMAIN);
    h.update(&want_digest.0).update(&unencoded.0);
    h.finish()
}
