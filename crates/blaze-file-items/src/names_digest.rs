//! Each entry's names digest ([`names_digests`]): what name resolution can observe of the entry
//! without its types (docs/v2/plan/design/read-addressed.md, "End state: what an edit costs",
//! the Surface row and piece 2). [`Outline::surface`](crate::Outline::surface) hashes these in
//! place of the signature digests, so an edit to a parameter's, a field's or a return type, a
//! bound, a where clause, a const's value or a doc comment leaves a file's surface equal and its
//! crate's names are not solved again; a rename, a visibility, an attribute, a variant, a struct's
//! form or a new member moves it.
//!
//! # What the name solver reads of an entry (blaze-scope task/lower.rs, `Lower::item`,
//! `Lower::assoc`, `Lower::attrs`, `Lower::vis`, `Lower::impl_header`, `Lower::fn_like_call`)
//!
//! | Field it reads | How | In the digest |
//! |---|---|---|
//! | `detail`'s kind | the arm taken, the key's kind tag | the kind byte |
//! | `name` | declared, keyed | its text |
//! | `visibility` | every declaration's `WrittenVis`, `pub(in path)`'s path | its form and the path's text |
//! | `attrs` (all but `Doc`) | `cfg`, `cfg_attr`, `derive`, `path`, `macro_use`, `macro_export`, `lang`, `proc_macro*`, `rustc_builtin_macro`, attribute macros | each one's text and whether inner |
//! | `body`'s first byte (struct, variant) | unit or tuple struct also a value; a braced variant is no value | the form byte |
//! | `Fn`/`Const`/`Static`'s `body_holds_items` | `BodySrc::of`: the body's blocks are scanned for items | the flag, and the whole digest when set |
//! | the whole text under an attribute macro or a derive | the call's input (`CallSrc::input`) | the whole digest ([`Item::hash`]) |
//! | `Impl`'s header (`impl_header`: the signature without its outer attributes) | the impl's trait path, self type and parameters | [`Item::signature_hash`] |
//! | `Use`'s `tree` | `use_leaves` | its text |
//! | `ExternCrate`'s `rename` | the name it binds | its form and text |
//! | `Mod`'s `inline`, `path` | the module's file | the flag and the path's text |
//! | `MacroRules`'s `exported` and body; `MacroDef`'s body | the rules' shapes, the macro's tokens | the whole digest |
//! | `MacroCall`'s `path` and `input` | the call's path and its tokens | the whole digest |
//! | `ExternBlock` | its entries are lowered in its place | its ABI's text |
//! | `Error`, `Unhandled` | a note | the whole digest |
//! | `range` (`ItemText::text`, `Member::text`, `Loc`) | where its text is: a position, not a name | no: positions are not in a surface (rule 71) |
//!
//! Not in it: parameter and return types, field types (`Field`'s `ty`, `default`), a variant's
//! discriminant, generics and their bounds, where clauses, bodies, a const's or static's type and
//! value, doc comments, comments and whitespace outside an attribute, and the entry's place in the
//! file. An entry whose text the solver hands on whole (a derive's or an attribute macro's input,
//! a macro's tokens, a body it scans) is digested whole, so nothing that entry's expansion can see
//! is left out (AGENTS.md rule 21, `keys-cover-every-input`).
//!
//! An attribute is taken to be an attribute macro, or to hold one (`cfg_attr`, `unsafe(..)`),
//! unless its name is [`inert`] (`blaze_surface::builtin_attrs`, the solver's own list) or a `rustc_`
//! name, or its path starts with a tool of `blaze_surface::tool_attrs::TOOLS`: a name missing from
//! the list only costs a solve, never an answer. Only entries the solver lowers one by one
//! (`Lower::item`: the file's, a module's and an extern block's) have an attribute macro called; a
//! trait's or an impl's member (`Lower::assoc`), a variant and a field have none called, so theirs
//! do not make the entry whole: a derive or an attribute macro on their owner does.
//!
//! The file's own inner attributes are no entry's: [`file_names_digest`] holds each one's text
//! but a doc comment's (blaze-scope task/lower.rs `file_attrs`: the root's `register_tool`,
//! `no_std`, `no_core`, a module's `cfg`, `macro_use`, `no_implicit_prelude`), and
//! [`Outline::surface`](crate::Outline::surface) hashes it first (names-attr-4).

use blaze_ids::{ContentId, Domain};
use blaze_own::AVec;

use crate::digest::{BATCH, BATCH_BYTES, Compress, Sink};
use blaze_surface::builtin_attrs::BUILTIN_ATTRS;
use blaze_surface::tool_attrs::TOOLS;
use blaze_surface::{Attr, AttrKind, Item, ItemDetail, Range, Rename, WrittenVis};

/// [`NAMES_DOMAIN`](crate::NAMES_DOMAIN), its context key derived once.
static NAMES: Domain = Domain::new(crate::NAMES_DOMAIN);

/// The domain of [`file_names_digest`].
static FILE_NAMES: Domain = Domain::new("blaze v2 file inner attributes names 1");

/// Whether an attribute named `name` makes the name solver read no item text: a built-in
/// attribute (`blaze_surface::builtin_attrs::BUILTIN_ATTRS`, the list the solver's lowering
/// reads), so the solver never calls it as an attribute macro and reads only the attribute's own
/// text, which the digest holds; or the prelude's `test` or `bench`, which the lowering reads
/// itself and keeps the item for. Left out on purpose: `cfg_attr` and `unsafe` (each holds another
/// attribute), `derive` and `derive_const` (the item is the derive's input), `global_allocator`
/// (the item is its expansion's input: blaze-expand global_allocator.rs writes the static back
/// with the allocator fns), `cfg_eval` and `define_opaque` (built-in attribute macros), and a
/// derive's helper (`default`).
pub(crate) fn inert(name: &str) -> bool {
    (BUILTIN_ATTRS.binary_search(&name).is_ok() && !matches!(name, "cfg_attr" | "unsafe")) || matches!(name, "test" | "bench")
}

/// Each entry of `items` (an outline of `text` in the edition whose hash byte is `edition`, its
/// `hash` and `signature_hash` set, its attributes in `attrs`) its names digest, in order, into
/// `out` (emptied first). The messages are copied into `scratch` and hashed side by side by `C`,
/// up to [`BATCH`] or [`BATCH_BYTES`] at a time, as the entries' other digests are
/// (crate::digest): the same bytes as one hasher an entry gives (`names_digest`, the definition,
/// in the tests), a message being short and a hasher's fixed cost most of a short one's.
pub(crate) fn names_digests<C: Compress>(
    text: &str,
    edition: u8,
    items: &[Item],
    attrs: &[Attr],
    out: &mut AVec<'_, ContentId>,
    scratch: &mut AVec<'_, u8>,
) {
    out.clear();
    out.reserve(items.len());
    scratch.clear();
    let mut ends = [0usize; BATCH + 1];
    let mut count = 0;
    for (at, item) in items.iter().enumerate() {
        let calls_macros = item.parent.is_none_or(|parent| lowered_one_by_one(items[parent.at()].detail));
        message(scratch, text, edition, item, &attrs[item.attrs.to_usize()], calls_macros);
        count += 1;
        ends[count] = scratch.len();
        if count == BATCH || scratch.len() >= BATCH_BYTES || at + 1 == items.len() {
            let mut inputs: [&[u8]; BATCH] = [&[]; BATCH];
            for (at, input) in inputs[..count].iter_mut().enumerate() {
                *input = &scratch[ends[at]..ends[at + 1]];
            }
            let mut digests = [[0u8; 32]; BATCH];
            C::derive_many(NAMES.context_key(), &inputs[..count], &mut digests[..count]);
            out.extend(digests[..count].iter().map(|digest| ContentId(*digest)));
            scratch.clear();
            count = 0;
        }
    }
}

/// The digest of a file's inner attributes `attrs` (its `file_attrs`) in `text`: each one's text
/// but a doc comment's, in order. What the name solver reads of them (names-attr-4): an edit to
/// `#![register_tool(x)]` moves it, a crate doc comment or blank lines between them do not.
pub(crate) fn file_names_digest(text: &str, attrs: &[Attr]) -> ContentId {
    let mut hasher = blaze_ids::Hasher::of(&FILE_NAMES);
    for attr in attrs {
        if attr.kind == AttrKind::Doc {
            continue;
        }
        hasher.part(attr.range.of(text).as_bytes());
    }
    hasher.finish()
}

/// Whether the solver lowers the entries written in an entry of `detail` one by one
/// (`Lower::item`), where an attribute macro on one is called with its text: a module's and an
/// extern block's. A trait's and an impl's members go through `Lower::assoc`, which calls none; a
/// variant's and a field's are not lowered at all. Any other owner is taken as one that does.
const fn lowered_one_by_one(detail: ItemDetail) -> bool {
    !matches!(
        detail,
        ItemDetail::Trait
            | ItemDetail::Impl
            | ItemDetail::Struct
            | ItemDetail::Enum
            | ItemDetail::Union
            | ItemDetail::Variant { .. }
    )
}

/// The names digest of `item`, an entry of an outline of `text`, whose attributes are `attrs`;
/// `calls_macros`: whether an attribute macro on it is called with its text. The definition:
/// [`message`] streamed through one hasher in [`NAMES_DOMAIN`](crate::NAMES_DOMAIN), which
/// [`names_digests`] is tested equal to.
#[cfg(test)]
pub(crate) fn names_digest(text: &str, edition: u8, item: &Item, attrs: &[Attr], calls_macros: bool) -> ContentId {
    let mut hasher = blaze_ids::Hasher::of(&NAMES);
    message(&mut hasher, text, edition, item, attrs, calls_macros);
    hasher.finish()
}

/// The message a names digest is the digest of, into `sink`: one definition, whether the bytes
/// are streamed through a hasher or copied to be hashed side by side with other entries'.
fn message(sink: &mut impl Sink, text: &str, edition: u8, item: &Item, attrs: &[Attr], calls_macros: bool) {
    let own = |range: Range| item.text(text, range).as_bytes();
    sink.part(&[edition, item.kind() as u8]);
    match item.name {
        Some(name) => sink.part(&[1]).part(own(name)),
        None => sink.part(&[0]),
    };
    match item.visibility {
        WrittenVis::Private => sink.part(&[0]),
        WrittenVis::Pub => sink.part(&[1]),
        WrittenVis::Crate => sink.part(&[2]),
        WrittenVis::Super => sink.part(&[3]),
        WrittenVis::SelfOnly => sink.part(&[4]),
        WrittenVis::In(path) => sink.part(&[5]).part(own(path)),
        WrittenVis::WithoutIn(path) => sink.part(&[6]).part(own(path)),
    };
    let mut whole = false;
    for attr in attrs {
        // `Lower::attrs` passes over a doc comment and `#[doc ..]`.
        if attr.kind == AttrKind::Doc {
            continue;
        }
        let written = item.text(text, attr.range);
        sink.part(&[u8::from(attr.inner)]).part(written.as_bytes());
        whole |= calls_macros && takes_the_item(attr.kind, written);
    }
    match item.detail {
        ItemDetail::Fn { body_holds_items }
        | ItemDetail::Const { body_holds_items }
        | ItemDetail::Static { body_holds_items } => {
            sink.part(&[u8::from(body_holds_items)]);
            // `BodySrc::of`: a body that holds items is scanned for the modules its blocks make.
            whole |= body_holds_items && crate::nested::may_write_block(item.range.of(text));
        }
        ItemDetail::Struct | ItemDetail::Variant { discriminant: _ } => {
            sink.part(&form(text, item));
        }
        ItemDetail::Enum
        | ItemDetail::Union
        | ItemDetail::Trait
        | ItemDetail::TraitAlias
        | ItemDetail::TypeAlias
        | ItemDetail::Field { ty: _, default: _ } => {}
        ItemDetail::Impl => {
            // `Lower::impl_header`: the header, every byte of it, is read for the impl's trait,
            // self type and parameters.
            sink.id(&item.signature_hash);
        }
        ItemDetail::Mod { inline, path } => {
            sink.part(&[u8::from(inline)]);
            optional(sink,path.map(own));
        }
        ItemDetail::Use { tree } => optional(sink,tree.map(own)),
        ItemDetail::ExternCrate { rename } => {
            match rename {
                Rename::Same => sink.part(&[0]),
                Rename::To(to) => sink.part(&[1]).part(own(to)),
                Rename::Underscore => sink.part(&[2]),
            };
        }
        ItemDetail::ExternBlock { abi } => optional(sink,abi.map(own)),
        ItemDetail::MacroRules { exported } => {
            sink.part(&[u8::from(exported)]);
            whole = true;
        }
        ItemDetail::MacroDef | ItemDetail::MacroCall { path: _, input: _ } | ItemDetail::Error => whole = true,
        ItemDetail::Unhandled { syntax } => {
            sink.part(&syntax.to_le_bytes());
            whole = true;
        }
    }
    if whole {
        sink.part(&[1]).id(&item.hash);
    } else {
        sink.part(&[0]);
    }
}

/// `bytes`, or that there are none, into `sink`.
fn optional(sink: &mut impl Sink, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => sink.part(&[1]).part(bytes),
        None => sink.part(&[0]),
    };
}

/// A struct's or a variant's form as the solver reads it (`Lower::item`'s `Struct` and `Enum`
/// arms): the first byte of its body, trimmed, `(` for a tuple form, `{` for a braced one, and
/// none for a unit one.
fn form(text: &str, item: &Item) -> [u8; 2] {
    match item.body {
        None => [0, 0],
        Some(body) => match item.text(text, body).trim_start().as_bytes().first() {
            None => [1, 0],
            Some(&first) => [2, first],
        },
    }
}

/// Whether an attribute of `kind` written `written` (`#[..]` or `#![..]`) may hand its entry's
/// whole text to a macro: a derive, an attribute macro, or one that holds another attribute.
fn takes_the_item(kind: AttrKind, written: &str) -> bool {
    match kind {
        AttrKind::Doc | AttrKind::Cfg | AttrKind::Path { literal: _ } | AttrKind::MacroExport => false,
        AttrKind::CfgAttr => true,
        AttrKind::Other => match head(written) {
            Head::Name(name) => !(name.starts_with("rustc_") || inert(name)),
            Head::Path(first) => !TOOLS.contains(&first),
            Head::Unread => true,
        },
    }
}

/// The path an attribute starts with, as far as [`takes_the_item`] needs it.
enum Head<'t> {
    /// One segment: its name.
    Name(&'t str),
    /// Two or more: the first.
    Path(&'t str),
    /// Neither read: an absolute path, a raw name or a form not expected.
    Unread,
}

/// The [`Head`] of `written`, an attribute's text from its `#`.
fn head(written: &str) -> Head<'_> {
    let Some(rest) = written.trim_start().strip_prefix('#') else { return Head::Unread };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('!').unwrap_or(rest).trim_start();
    let Some(rest) = rest.strip_prefix('[') else { return Head::Unread };
    let rest = rest.trim_start();
    let len = rest.bytes().take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_').count();
    if len == 0 {
        return Head::Unread;
    }
    let (first, after) = rest.split_at(len);
    if after.starts_with('#') {
        // `r#name`: a raw name.
        return Head::Unread;
    }
    if after.trim_start().starts_with("::") { Head::Path(first) } else { Head::Name(first) }
}

#[cfg(test)]
mod tests {
    use std::{format, string::String};

    use blaze_surface::{Edition, ItemIndex};

    use super::*;

    /// Every form [`message`] has an arm for, then enough entries to fill more than one batch,
    /// one of them with an attribute longer than a batch's bytes.
    fn fixture() -> String {
        let mut text = String::from(
            "#![no_std]\n\
             extern crate alloc as a;\nextern crate core as _;\nextern crate std;\n\
             pub use a::{b, c as d};\npub(crate) mod m { pub(super) fn f() {} pub(in crate::m) struct U; }\n\
             #[path = \"x.rs\"] mod n;\n\
             #[derive(Clone)] pub struct S { pub a: u8, b: u16 }\nstruct T(u8);\nstruct V;\n\
             #[repr(u8)] enum E { A, B(u8), C { x: u8 } = 3 }\nunion W { a: u8 }\n\
             trait Tr { type A; const C: u8; fn f(&self); }\ntrait Al = Tr;\n\
             impl<X: Tr> Tr for X { type A = u8; const C: u8 = 1; fn f(&self) {} }\n\
             type Ty = u8;\nconst K: u8 = { struct In; 1 };\nstatic ST: u8 = 2;\n\
             extern \"C\" { fn ext(); static EXT: u8; }\n\
             #[macro_export] macro_rules! mr { () => {}; }\nmacro md() {}\nmr!();\n\
             #[tokio::main] fn attr_macro() {}\n#[rustfmt::skip] fn tool() {}\n#[cfg_attr(a, b)] fn held() {}\n\
             fn holds() { struct Inner; }\n/// docs\n#[inline] fn docs() {}\n",
        );
        text.push_str(&format!("#[doc(alias = \"{}\")] fn long() {{}}\n", "x".repeat(BATCH_BYTES + 7)));
        for at in 0..3 * BATCH + 5 {
            text.push_str(&format!("pub fn f{at}(a: u8) -> u8 {{ a }}\n"));
        }
        text
    }

    /// The names digests hashed side by side are, entry for entry, the definition: each
    /// entry's message through one hasher of its own. An outline's are made by
    /// [`names_digests`], in batches that end at [`BATCH`] entries, at [`BATCH_BYTES`] and at
    /// the last entry; the fixture crosses each of the three.
    #[test]
    fn side_by_side_is_one_hasher_an_entry() {
        let text = fixture();
        let outline = crate::outline(&text, Edition::E2024, blaze_own::testing::heap().alloc());
        assert!(outline.items().len() > 3 * BATCH + 40, "{} entries", outline.items().len());
        assert!(outline.errors().is_empty(), "{:?}", outline.errors());
        for (at, item) in outline.items().iter().enumerate() {
            let calls_macros =
                item.parent.is_none_or(|parent| lowered_one_by_one(outline.item(parent).detail));
            let one = names_digest(&text, Edition::E2024 as u8, item, outline.attrs(item), calls_macros);
            let index = ItemIndex(u32::try_from(at).expect("a small fixture"));
            assert_eq!(outline.names_digest(index), one, "entry {at}: {item:?}");
        }
    }
}
