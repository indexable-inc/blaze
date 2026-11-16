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
//! unless its name is one of [`INERT`] (a subset of blaze-scope's `BUILTIN_ATTRS`) or a `rustc_`
//! name, or its path starts with a tool of `blaze_surface::tool_attrs::TOOLS`: a name missing from
//! the list only costs a solve, never an answer. Only entries the solver lowers one by one
//! (`Lower::item`: the file's, a module's and an extern block's) have an attribute macro called; a
//! trait's or an impl's member (`Lower::assoc`), a variant and a field have none called, so theirs
//! do not make the entry whole: a derive or an attribute macro on their owner does.

use blaze_ids::{ContentId, Domain, Hasher};
use blaze_own::AVec;
use blaze_surface::tool_attrs::TOOLS;
use blaze_surface::{Attr, AttrKind, Item, ItemDetail, Range, Rename, WrittenVis};

/// [`NAMES_DOMAIN`](crate::NAMES_DOMAIN), its context key derived once.
static NAMES: Domain = Domain::new(crate::NAMES_DOMAIN);

/// Attribute names the name solver reads no item text for: each is in blaze-scope's
/// `BUILTIN_ATTRS` (task/lower.rs), so the solver never calls it as an attribute macro, and what it
/// reads of one is the attribute's own text, which the digest holds. Left out on purpose: `derive`,
/// `derive_const` (the item is the derive's input), `cfg_attr` and `unsafe` (each holds another
/// attribute) and `cfg_eval` (a built-in attribute macro).
pub(crate) const INERT: &[&str] = &[
    "align", "allow", "allow_internal_unsafe", "allow_internal_unstable", "automatically_derived",
    "cfg", "cold", "collapse_debuginfo", "const_trait", "coverage", "crate_name", "crate_type",
    "debugger_visualizer", "default", "deny", "deprecated", "diagnostic", "doc", "expect",
    "export_name", "feature", "ffi_const", "ffi_pure", "forbid", "fundamental", "global_allocator",
    "ignore", "inline", "instruction_set", "lang", "link", "link_name", "link_ordinal", "link_section",
    "linkage", "macro_export", "macro_use", "marker", "may_dangle", "must_not_suspend", "must_use",
    "naked", "no_builtins", "no_core", "no_implicit_prelude", "no_link", "no_main", "no_mangle",
    "no_std", "non_exhaustive", "optimize", "panic_handler", "path", "prelude_import", "proc_macro",
    "proc_macro_attribute", "proc_macro_derive", "recursion_limit", "register_tool", "repr",
    "should_panic", "stable", "target_feature", "test", "thread_local", "track_caller",
    "type_length_limit", "unstable", "used", "warn", "windows_subsystem",
];

/// Each entry of `items` (an outline of `text` in the edition whose hash byte is `edition`, its
/// `hash` and `signature_hash` set, its attributes in `attrs`) its names digest, in order, into
/// `out` (emptied first).
pub(crate) fn names_digests(text: &str, edition: u8, items: &[Item], attrs: &[Attr], out: &mut AVec<'_, ContentId>) {
    out.clear();
    out.reserve(items.len());
    for item in items {
        let calls_macros = item.parent.is_none_or(|parent| lowered_one_by_one(items[parent.at()].detail));
        out.push(names_digest(text, edition, item, &attrs[item.attrs.to_usize()], calls_macros));
    }
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
/// `calls_macros`: whether an attribute macro on it is called with its text.
pub(crate) fn names_digest(text: &str, edition: u8, item: &Item, attrs: &[Attr], calls_macros: bool) -> ContentId {
    let own = |range: Range| item.text(text, range).as_bytes();
    let mut hasher = Hasher::of(&NAMES);
    hasher.part(&[edition, item.kind() as u8]);
    match item.name {
        Some(name) => hasher.part(&[1]).part(own(name)),
        None => hasher.part(&[0]),
    };
    match item.visibility {
        WrittenVis::Private => hasher.part(&[0]),
        WrittenVis::Pub => hasher.part(&[1]),
        WrittenVis::Crate => hasher.part(&[2]),
        WrittenVis::Super => hasher.part(&[3]),
        WrittenVis::SelfOnly => hasher.part(&[4]),
        WrittenVis::In(path) => hasher.part(&[5]).part(own(path)),
        WrittenVis::WithoutIn(path) => hasher.part(&[6]).part(own(path)),
    };
    let mut whole = false;
    for attr in attrs {
        // `Lower::attrs` passes over a doc comment and `#[doc ..]`.
        if attr.kind == AttrKind::Doc {
            continue;
        }
        let written = item.text(text, attr.range);
        hasher.part(&[u8::from(attr.inner)]).part(written.as_bytes());
        whole |= calls_macros && takes_the_item(attr.kind, written);
    }
    match item.detail {
        ItemDetail::Fn { body_holds_items }
        | ItemDetail::Const { body_holds_items }
        | ItemDetail::Static { body_holds_items } => {
            hasher.part(&[u8::from(body_holds_items)]);
            // `BodySrc::of`: a body that holds items is scanned for the modules its blocks make.
            whole |= body_holds_items;
        }
        ItemDetail::Struct | ItemDetail::Variant { discriminant: _ } => {
            hasher.part(&form(text, item));
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
            hasher.id(&item.signature_hash);
        }
        ItemDetail::Mod { inline, path } => {
            hasher.part(&[u8::from(inline)]);
            optional(&mut hasher, path.map(own));
        }
        ItemDetail::Use { tree } => optional(&mut hasher, tree.map(own)),
        ItemDetail::ExternCrate { rename } => {
            match rename {
                Rename::Same => hasher.part(&[0]),
                Rename::To(to) => hasher.part(&[1]).part(own(to)),
                Rename::Underscore => hasher.part(&[2]),
            };
        }
        ItemDetail::ExternBlock { abi } => optional(&mut hasher, abi.map(own)),
        ItemDetail::MacroRules { exported } => {
            hasher.part(&[u8::from(exported)]);
            whole = true;
        }
        ItemDetail::MacroDef | ItemDetail::MacroCall { path: _, input: _ } | ItemDetail::Error => whole = true,
        ItemDetail::Unhandled { syntax } => {
            hasher.part(&syntax.to_le_bytes());
            whole = true;
        }
    }
    if whole {
        hasher.part(&[1]).id(&item.hash);
    } else {
        hasher.part(&[0]);
    }
    hasher.finish()
}

/// `bytes`, or that there are none, into `hasher`.
fn optional(hasher: &mut Hasher, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => hasher.part(&[1]).part(bytes),
        None => hasher.part(&[0]),
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
            Head::Name(name) => !(name.starts_with("rustc_") || INERT.contains(&name)),
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
