//! `cbor_field_order` (in `blaze::internal`): a struct, or a struct variant of an enum, that
//! derives serde's `Serialize` in a module marked `#![blaze::canonical_cbor]` with its fields
//! declared out of their keys' canonical order. RFC 8949's core deterministic CBOR
//! (`blaze_protocol::cbor`) writes a map's keys in the bytewise order of their encodings
//! (`key_order` there), which for text keys is shorter first, then bytewise; a type declared in
//! that order is written as it comes, with nothing to sort, and one declared in another has its
//! entries found again, sorted and copied at its end. blaze-api promised the order in its module
//! doc and seven of its structs broke it; only a test of the codec found them (the "written out
//! of order" assertion of `crates/blaze-api/src/frames/identity.rs`).
//!
//! The derive is serde's own (crates.io `serde_derive`, through `serde`), or blaze's
//! (`blaze_serde`'s re-export of `blaze_serde_derive`), resolved by name resolution as
//! `lenient_deserialize` resolves `Deserialize`; serde's derive is not the tree's to change, so
//! the promise is checked here, at the definition, and not as an error of the derive.
//!
//! The scope is opt-in: an item is judged only when its module, or an ancestor of it (the crate
//! root included), carries the inner attribute `#![blaze::canonical_cbor]` (a blaze extension,
//! `AttrFlags::CANONICAL_CBOR`, inert under rustc with `#![feature(register_tool,
//! custom_inner_attributes)]` and `#![register_tool(blaze)]` at the crate root), read as
//! `sans_io_core::core_of` reads `#![blaze::sans_io]`.
//!
//! A field's key is its `#[serde(rename = "..")]` (or the `serialize` of `rename(..)`), else its
//! name (`r#` taken off) after the case rule of the container: a struct's `rename_all`, a
//! variant's own `rename_all`, else its enum's `rename_all_fields` (the `serialize` side of
//! each). A field `#[serde(skip)]` or `#[serde(skip_serializing)]` is no key, and keeps its
//! place. Nothing is found where the key can't be told from the syntax:
//!
//! - a `#[serde(flatten)]` field: its keys are another type's, merged into this map;
//! - `#[serde(transparent)]` or `#[serde(into = "..")]`: the value is written as another type;
//! - a case rule serde names but this lint doesn't model (it models `lowercase`, `UPPERCASE`,
//!   `PascalCase`, `camelCase`, `snake_case`, `SCREAMING_SNAKE_CASE`, `kebab-case` and
//!   `SCREAMING-KEBAB-CASE`), or a rename that is not a plain string literal;
//! - a `#[serde(..)]` inside a `cfg_attr`, whose key depends on the configuration;
//! - a tuple or unit struct, a tuple or unit variant: no keys.
//!
//! An internally tagged enum's or struct's tag (`#[serde(tag = "..")]` with no `content`) is
//! written first by the derive wherever its key sorts; the lint judges the fields among
//! themselves, which a reorder can fix, and not the tag.
//!
//! Not checked: test code, an item a macro wrote (`blaze_protocol::in_key_order!` is one; its
//! `Serialize` is the macro's), an item declared in a function body.
//!
//! The fix moves each field's whole text (its attributes, doc comments and the comments attached
//! to it) into the canonical order, a skipped field keeping its place. It is machine-applicable,
//! except where it may change what the code means: when the type derives `PartialOrd`, `Ord` or
//! `Hash` (a derived comparison or hash follows the declaration order, so the reorder changes
//! it), or when a comment between fields belongs to neither (it stays where it is while the
//! fields move); then it is offered as maybe-incorrect, with a note saying which.

use std::cmp::Ordering;

use hir_def::{AttrDefId, attrs::AttrFlags};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::{Lint, facts};
use crate::{
    check::{self, Cx},
    diag::Applicability,
};

const LINT: Lint = Lint::CborFieldOrder;

/// The crates whose `Serialize` derive this lint knows: serde's, through `serde` or its
/// `serde_derive`, and blaze's (`blaze_serde`'s re-export of `blaze_serde_derive`).
const DERIVE_CRATES: &[&str] = &["serde_derive", "serde", "blaze_serde_derive"];

/// The derives whose meaning follows the fields' declaration order.
const ORDERED_DERIVES: &[&str] = &["PartialOrd", "Ord", "Hash"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.in_file()
        || !cx.is_written(adt.syntax())
        || cx.is_in_test(adt.syntax())
    {
        return;
    }
    // The syntax first: name resolution only for an item with fields out of order.
    let found = out_of_order(adt);
    if found.is_empty() || !serializes(cx, adt) || !canonical(cx, adt) {
        return;
    }
    let ordered = ordered_derive(cx, adt);
    for list in found {
        report(cx, list, ordered);
    }
}

/// One list of fields, a struct's or a struct variant's, out of its keys' order.
struct Unordered<'t> {
    /// Where the finding's level is asked: the struct, or the variant.
    at: ast::Node<'t>,
    /// What the message calls it: "struct `Foo`", "struct variant `Kind::Bar`".
    what: String,
    list: ast::RecordFieldList<'t>,
    /// The fields that are keys, in declaration order, each with its key.
    keyed: Vec<Keyed<'t>>,
}

struct Keyed<'t> {
    field: ast::RecordField<'t>,
    key: String,
}

/// Each list of `adt`'s fields whose keys the syntax tells and that is out of their order.
fn out_of_order<'t>(adt: ast::Adt<'t>) -> Vec<Unordered<'t>> {
    // alloc: the lists found, few, reported at the edge
    let mut found = Vec::new();
    if conditional(adt.attrs()) || says(adt.attrs(), "transparent") || says(adt.attrs(), "into") {
        return found;
    }
    let Some(name) = adt.name() else { return found };
    match adt {
        ast::Adt::Struct(it) => {
            let Some(ast::FieldList::RecordFieldList(list)) = it.field_list() else {
                return found;
            };
            let Ok(case) = rule(adt.attrs(), "rename_all") else { return found };
            if let Some(keyed) = judged(list, case) {
                // alloc: the message's name
                let what = format!("struct `{}`", name.text());
                found.push(Unordered { at: adt.syntax(), what, list, keyed });
            }
        }
        ast::Adt::Enum(it) => {
            let Ok(fields_case) = rule(adt.attrs(), "rename_all_fields") else { return found };
            for variant in it.variant_list().into_iter().flat_map(|it| it.variants()) {
                let Some(ast::FieldList::RecordFieldList(list)) = variant.field_list() else {
                    continue;
                };
                if conditional(variant.attrs())
                    || says(variant.attrs(), "skip")
                    || says(variant.attrs(), "skip_serializing")
                {
                    continue;
                }
                let Ok(own) = rule(variant.attrs(), "rename_all") else { continue };
                let Some(keyed) = judged(list, own.or(fields_case)) else { continue };
                let Some(variant_name) = variant.name() else { continue };
                // alloc: the message's name
                let what = format!("struct variant `{}::{}`", name.text(), variant_name.text());
                found.push(Unordered { at: variant.syntax(), what, list, keyed });
            }
        }
        ast::Adt::Union(_) => {}
    }
    found
}

/// The keyed fields of `list` under the case rule `case`, when every key is told by the syntax
/// and two of them are out of order.
fn judged(list: ast::RecordFieldList<'_>, case: Option<Case>) -> Option<Vec<Keyed<'_>>> {
    // alloc: the list's keys, compared and reported at the edge
    let mut keyed = Vec::new();
    for field in list.fields() {
        if conditional(field.attrs()) || says(field.attrs(), "flatten") {
            return None;
        }
        if says(field.attrs(), "skip") || says(field.attrs(), "skip_serializing") {
            continue;
        }
        let key = match renamed(field.attrs())? {
            Some(it) => it.to_owned(), // alloc: a key, compared
            None => {
                let name = field.name()?.text();
                let name = name.strip_prefix("r#").unwrap_or(name);
                match case {
                    Some(case) => case.apply(name),
                    None => name.to_owned(), // alloc: as above
                }
            }
        };
        keyed.push(Keyed { field, key });
    }
    let unordered =
        keyed.windows(2).any(|it| name_order(&it[0].key, &it[1].key) == Ordering::Greater);
    unordered.then_some(keyed)
}

/// The order of two text keys' encodings, `blaze_protocol::cbor`'s `name_order`: a text head
/// grows with its length, so a shorter key comes first and keys of one length go bytewise.
fn name_order(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

fn report<'t>(cx: &mut Cx<'_, 't>, found: Unordered<'t>, ordered: Option<&'static str>) {
    let Unordered { at, what, list, keyed } = found;
    // alloc: the canonical order of the list's slots
    let mut order: Vec<usize> = (0..keyed.len()).collect();
    order.sort_by(|&a, &b| name_order(&keyed[a].key, &keyed[b].key));
    let Some(first) = (0..order.len()).find(|&it| order[it] != it) else { return };
    let (moved, before) = (&keyed[order[first]], &keyed[first]);
    let (Some(moved_name), Some(before_name)) = (moved.field.name(), before.field.name()) else {
        return;
    };
    let Some(span_range) = cx.range(moved_name.syntax()) else { return };
    let message = format!(
        "{what} declares its fields out of their keys' order: `{}` goes before `{}`: its key `{}` sorts before `{}` (shorter first, then bytewise)",
        moved_name.text(),
        before_name.text(),
        moved.key,
        before.key,
    );
    // alloc: a message's part
    let note = "the deterministic CBOR writes a map's keys in that order; a type declared in it \
                is written as it comes, one declared in another has its entries sorted and copied"
        .to_owned();
    let mut parts = Some(Vec::new()); // alloc: the fix's parts, one per field moved
    for (slot, &from) in order.iter().enumerate() {
        if slot == from {
            continue;
        }
        match (cx.range(keyed[slot].field.syntax()), cx.range(keyed[from].field.syntax())) {
            (Some(to), Some(text)) => {
                let text = cx.snippet(text).to_owned(); // alloc: the fix's text
                if let Some(parts) = &mut parts {
                    parts.push((cx.span(to), text));
                }
            }
            _ => parts = None,
        }
    }
    let stray = stray_comment(list);
    let span = cx.span(span_range);
    let diagnostic = cx.blaze(LINT, at, span, message);
    diagnostic.note(note);
    if let Some(parts) = parts {
        let applicability = match (ordered, stray) {
            (None, false) => Applicability::MachineApplicable,
            _ => Applicability::MaybeIncorrect,
        };
        if let Some(derive) = ordered {
            diagnostic.note(format!(
                "the type derives `{derive}`, which follows the fields' declaration order: the reorder changes what it computes"
            ));
        }
        if stray {
            diagnostic.note(
                "a comment between the fields belongs to neither; it stays where it is while the fields move",
            );
        }
        diagnostic.multipart_suggestion(
            "declare the fields in their keys' order",
            parts,
            applicability,
        );
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// Whether a comment is a child of `list` itself: one of no field's, which a reorder leaves
/// where it is.
fn stray_comment(list: ast::RecordFieldList<'_>) -> bool {
    list.syntax().children_with_tokens().any(|it| match it {
        NodeOrToken::Token(token) => token.kind() == SyntaxKind::COMMENT,
        NodeOrToken::Node(_) => false,
    })
}

/// Whether `adt` has a derive that name resolution takes to a `Serialize` derive of
/// [`DERIVE_CRATES`].
fn serializes<'t>(cx: &Cx<'_, 't>, adt: ast::Adt<'t>) -> bool {
    let db = cx.db();
    adt.attrs().filter(|it| it.simple_name() == Some("derive")).any(|attr| {
        cx.derive_macros(attr).into_iter().flatten().any(|found| {
            let krate = found.module(db).krate(db);
            found.name(db).as_str() == "Serialize"
                && DERIVE_CRATES.iter().any(|it| facts::crate_is(db, krate, it))
        })
    })
}

/// The first of [`ORDERED_DERIVES`] `adt` derives: by the macro a derive path resolves to, else
/// (unresolved) by the path's last segment.
fn ordered_derive<'t>(cx: &Cx<'_, 't>, adt: ast::Adt<'t>) -> Option<&'static str> {
    let db = cx.db();
    adt.attrs().filter(|it| it.simple_name() == Some("derive")).find_map(|attr| {
        let macros = cx.derive_macros(attr);
        let paths = check::derive_paths(attr);
        paths.iter().enumerate().find_map(|(at, (_, path))| {
            let named = |name: &str| ORDERED_DERIVES.iter().copied().find(|it| *it == name);
            match macros.get(at).copied().flatten() {
                Some(found) => named(found.name(db).as_str()),
                None => named(path.rsplit("::").next().unwrap_or(path.as_str()).trim()),
            }
        })
    })
}

/// Whether `adt`'s module, or an ancestor of it, is marked `#![blaze::canonical_cbor]`.
fn canonical<'t>(cx: &Cx<'_, 't>, adt: ast::Adt<'t>) -> bool {
    let db = cx.db();
    let Some(def) = cx.adt_def(adt) else { return false };
    let mut module = def.module(db);
    loop {
        if AttrFlags::query(db, AttrDefId::ModuleId(module.into()))
            .contains(AttrFlags::CANONICAL_CBOR)
        {
            return true;
        }
        let Some(parent) = module.parent(db) else { return false };
        module = parent;
    }
}

/// A case rule of serde's `rename_all` (`RenameRule`), as it applies to a field's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    /// `lowercase` and `snake_case`: a field's name is already both.
    Same,
    Upper,
    Pascal,
    Camel,
    Kebab,
    ScreamingKebab,
}

impl Case {
    fn of(name: &str) -> Option<Case> {
        Some(match name {
            "lowercase" | "snake_case" => Case::Same,
            "UPPERCASE" | "SCREAMING_SNAKE_CASE" => Case::Upper,
            "PascalCase" => Case::Pascal,
            "camelCase" => Case::Camel,
            "kebab-case" => Case::Kebab,
            "SCREAMING-KEBAB-CASE" => Case::ScreamingKebab,
            _ => return None,
        })
    }

    /// serde's `RenameRule::apply_to_field`.
    fn apply(self, field: &str) -> String {
        match self {
            Case::Same => field.to_owned(), // alloc: a key, compared
            Case::Upper => field.to_ascii_uppercase(), // alloc: as above
            Case::Pascal | Case::Camel => {
                // alloc: as above
                let mut out = String::with_capacity(field.len());
                let mut capitalize = true;
                for ch in field.chars() {
                    if ch == '_' {
                        capitalize = true;
                    } else if capitalize {
                        out.push(ch.to_ascii_uppercase());
                        capitalize = false;
                    } else {
                        out.push(ch);
                    }
                }
                // camelCase is PascalCase with its first character lowered, as serde makes it.
                if self == Case::Camel
                    && let Some(first) = out.get_mut(..1)
                {
                    first.make_ascii_lowercase();
                }
                out
            }
            Case::Kebab => field.replace('_', "-"), // alloc: as above
            Case::ScreamingKebab => field.to_ascii_uppercase().replace('_', "-"), // alloc: as above
        }
    }
}

/// The serialize-side case rule `word` (`rename_all`, `rename_all_fields`) of `attrs`'
/// `#[serde(..)]`: `Ok(None)` when there is none, `Err` when there is one this lint can't read.
fn rule<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>, word: &str) -> Result<Option<Case>, ()> {
    let mut found = None;
    for item in serde_items(attrs).filter(|it| it.word == word) {
        let Some(name) = serialize_side(&item)? else { continue };
        found = Some(Case::of(name).ok_or(())?);
    }
    Ok(found)
}

/// The serialize-side `rename` of a field's `attrs`: `Some(None)` when there is none, `None`
/// when there is one this lint can't read.
fn renamed<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>) -> Option<Option<&'t str>> {
    let mut found = None;
    for item in serde_items(attrs).filter(|it| it.word == "rename") {
        if let Some(name) = serialize_side(&item).ok()? {
            found = Some(name);
        }
    }
    Some(found)
}

/// The string `item` gives the serialize side: `x` of `word = "x"` or of `word(serialize =
/// "x")`; `Ok(None)` for `word(deserialize = "..")`; `Err` for a shape this lint can't read.
fn serialize_side<'t>(item: &Item<'t>) -> Result<Option<&'t str>, ()> {
    match (item.value, item.nested) {
        (Some(value), None) => unquote(value).map(Some).ok_or(()),
        (None, Some(nested)) => {
            let mut found = None;
            for inner in items(nested) {
                match (inner.word, inner.value) {
                    ("serialize", Some(value)) => found = Some(unquote(value).ok_or(())?),
                    ("deserialize", Some(_)) => {}
                    _ => return Err(()),
                }
            }
            Ok(found)
        }
        _ => Err(()),
    }
}

/// The text of a plain string literal: `x` of `"x"`; `None` for an escape or a raw string.
fn unquote(literal: &str) -> Option<&str> {
    let text = literal.strip_prefix('"')?.strip_suffix('"')?;
    (!text.contains('\\')).then_some(text)
}

/// Whether a `#[serde(..)]` of `attrs` is inside a `cfg_attr`: its effect depends on the
/// configuration.
fn conditional<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>) -> bool {
    attrs.filter_map(|it| it.meta()).any(|meta| {
        matches!(meta, ast::Meta::CfgAttrMeta(_))
            && meta.skip_cfg_attrs().any(|it| it.as_simple_call().is_some_and(|(name, _)| name == "serde"))
    })
}

/// Whether a `#[serde(..)]` of `attrs` has an item `word` (`flatten`, `skip`, `into = ".."`).
fn says<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>, word: &str) -> bool {
    serde_items(attrs).any(|it| it.word == word)
}

/// The items of every `#[serde(..)]` of `attrs` (not those in a `cfg_attr`: see
/// [`conditional`]).
fn serde_items<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>) -> impl Iterator<Item = Item<'t>> {
    attrs
        .filter_map(|it| it.meta())
        .filter_map(|meta| meta.as_simple_call())
        .filter(|(name, _)| *name == "serde")
        .flat_map(|(_, tt)| items(tt))
}

/// One comma-separated item of an attribute's token tree: `word`, `word = "value"` or
/// `word(..)`.
#[derive(Clone, Copy)]
struct Item<'t> {
    word: &'t str,
    /// The literal's text after `=`, quotes included.
    value: Option<&'t str>,
    nested: Option<ast::TokenTree<'t>>,
}

/// The items of `tt`, `#[serde(..)]`'s list.
fn items(tt: ast::TokenTree<'_>) -> Vec<Item<'_>> {
    // alloc: an attribute's few items
    let mut out = Vec::new();
    let mut current: Option<Item<'_>> = None;
    for child in tt.token_trees_and_tokens() {
        match child {
            NodeOrToken::Token(token) => match token.kind() {
                kind if kind.is_trivia() => {}
                SyntaxKind::L_PAREN | SyntaxKind::R_PAREN => {}
                SyntaxKind::COMMA => out.extend(current.take()),
                SyntaxKind::EQ => {}
                _ => match &mut current {
                    None => current = Some(Item { word: token.text(), value: None, nested: None }),
                    Some(item) => item.value = Some(token.text()),
                },
            },
            NodeOrToken::Node(nested) => {
                if let Some(item) = &mut current {
                    item.nested = Some(nested);
                }
            }
        }
    }
    out.extend(current);
    out
}

#[cfg(test)]
mod tests {
    use base_db::FileId;
    use hir_expand::fixture;

    use crate::{
        check::{Setup, check},
        conf::Conf,
        fix::fix,
        level::CommandLine,
        lints::disallowed_tests::give_derives,
        tests::lint_given,
    };

    const FLAGS: &str = "-A clippy::all --warn=blaze::cbor_field_order";

    /// `serde` and `blaze_serde`, each re-exporting a derive crate's `Serialize` and
    /// `Deserialize` (which make nothing, [`give_derives`]) that take `#[serde(..)]` as their
    /// helper attribute.
    const CRATES: &str = r#"
//- minicore: derive, ord, hash
//- /serde_derive/lib.rs crate:proc-macro:serde_derive
#[proc_macro_derive(Serialize, attributes(serde))]
pub fn serialize(item: TokenStream) -> TokenStream {
    item
}
#[proc_macro_derive(Deserialize, attributes(serde))]
pub fn deserialize(item: TokenStream) -> TokenStream {
    item
}
//- /serde/lib.rs crate:serde deps:serde_derive
pub use serde_derive::{Deserialize, Serialize};
//- /blaze_serde_derive/lib.rs crate:proc-macro:blaze_serde_derive
#[proc_macro_derive(Serialize, attributes(serde))]
pub fn serialize(item: TokenStream) -> TokenStream {
    item
}
#[proc_macro_derive(Deserialize, attributes(serde))]
pub fn deserialize(item: TokenStream) -> TokenStream {
    item
}
//- /blaze_serde/lib.rs crate:blaze_serde deps:blaze_serde_derive
pub use blaze_serde_derive::{Deserialize, Serialize};
//- /src/lib.rs crate:main deps:serde,blaze_serde
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
"#;

    fn fixture_of(main: &str) -> String {
        format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'))
    }

    fn found(main: &str) -> String {
        lint_given(&fixture_of(main), FLAGS, Conf::default(), give_derives)
    }

    const MESSAGE: &str = "declares its fields out of their keys' order";

    /// The bug in miniature, blaze-api's `Bug` declared as it read before: `kind` before `at`.
    /// Found, too: a struct variant through `blaze_serde`, a rename that moves a key, a case rule
    /// that does, a raw identifier's key. Not found: the same fields in order, a skipped field
    /// out of place, a `flatten`, `transparent`, a tuple struct, an unmodelled case rule, a
    /// derive of only `Deserialize`, and test code.
    #[test]
    fn fields_out_of_their_keys_order_are_found_in_a_canonical_module() {
        let out = found(
            r#"
#![blaze::canonical_cbor]
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct Bug {
    pub kind: u64,
    pub at: u64,
    pub what: u64,
}

#[derive(blaze_serde::Serialize)]
pub enum Event {
    Started,
    Ended { status: u32, id: u64 },
}

#[derive(Serialize)]
pub struct Renamed {
    #[serde(rename = "zz")]
    pub a: u32,
    pub b: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Kebab {
    pub a_b: u32,
    pub ab_c: u32,
    pub abc: u32,
}

#[derive(Serialize)]
pub struct Raw {
    pub r#type: u32,
    pub id: u32,
}

#[derive(Serialize)]
pub struct InOrder {
    pub at: u64,
    pub kind: u64,
    pub what: u64,
}

#[derive(Serialize)]
pub struct Skipped {
    pub at: u32,
    #[serde(skip)]
    pub cache: u32,
    pub id: u32,
}

#[derive(Serialize)]
pub struct Flat {
    pub kind: u32,
    #[serde(flatten)]
    pub rest: InOrder,
}

#[derive(Serialize)]
#[serde(transparent)]
pub struct Wrapper {
    pub kind: u32,
}

#[derive(Serialize)]
pub struct Tuple(pub u64, pub u32);

#[derive(Serialize)]
#[serde(rename_all = "Train-Case")]
pub struct Unmodelled {
    pub kind: u32,
    pub at: u32,
}

#[derive(Deserialize)]
pub struct ReadOnly {
    pub kind: u32,
    pub at: u32,
}

#[cfg(test)]
mod tests {
    #[derive(serde::Serialize)]
    pub struct InTest {
        pub kind: u32,
        pub at: u32,
    }
}
"#,
        );
        assert_eq!(out.matches(MESSAGE).count(), 5, "{out}");
        assert!(
            out.contains("struct `Bug` declares its fields out of their keys' order: `at` goes before `kind`"),
            "{out}"
        );
        assert!(
            out.contains("struct variant `Event::Ended` declares its fields out of their keys' order: `id` goes before `status`"),
            "{out}"
        );
        assert!(out.contains("`b` goes before `a`: its key `b` sorts before `zz`"), "{out}");
        assert!(out.contains("`abc` goes before `ab_c`: its key `abc` sorts before `ab-c`"), "{out}");
        assert!(out.contains("`id` goes before `r#type`: its key `id` sorts before `type`"), "{out}");
        for name in
            ["InOrder", "Skipped", "Flat", "Wrapper", "Tuple", "Unmodelled", "ReadOnly", "InTest"]
        {
            assert!(!out.contains(&format!("`{name}` declares")), "`{name}` is not found:\n{out}");
        }
    }

    /// The near miss: the same struct out of order in a module with no `#![blaze::canonical_cbor]`
    /// above it is not judged; the marker on a module covers its children.
    #[test]
    fn only_a_marked_module_and_its_children_are_judged() {
        let out = found(
            r#"
pub mod wire {
    #![blaze::canonical_cbor]
    pub mod inner {
        #[derive(serde::Serialize)]
        pub struct Marked {
            pub kind: u32,
            pub at: u32,
        }
    }
}
pub mod other {
    #[derive(serde::Serialize)]
    pub struct Unmarked {
        pub kind: u32,
        pub at: u32,
    }
}
"#,
        );
        assert_eq!(out.matches(MESSAGE).count(), 1, "{out}");
        assert!(out.contains("struct `Marked`"), "{out}");
    }

    /// An expectation on the item is met by the finding: no finding, and no unfulfilled
    /// expectation.
    #[test]
    fn an_expect_on_the_item_is_fulfilled() {
        let out = found(
            r#"
#![blaze::canonical_cbor]
#[expect(blaze::cbor_field_order, reason = "the derived `Ord` compares `kind` first")]
#[derive(serde::Serialize)]
pub struct Sorted {
    pub kind: u32,
    pub at: u32,
}
"#,
        );
        assert_eq!(out.matches(MESSAGE).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "the expectation is met:\n{out}");
    }

    /// A type that derives `Ord` is offered the reorder as maybe-incorrect, with the note that
    /// its derived order changes.
    #[test]
    fn a_derived_ord_makes_the_reorder_maybe_incorrect() {
        let out = found(
            r#"
#![blaze::canonical_cbor]
#[derive(serde::Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Sorted {
    pub kind: u32,
    pub at: u32,
}
"#,
        );
        assert_eq!(out.matches(MESSAGE).count(), 1, "{out}");
        assert!(out.contains("the type derives `PartialOrd`"), "{out}");
    }

    /// The fix moves each field's whole text, its doc comment and attributes with it, a skipped
    /// field keeping its place; the fixed file has nothing left for the lint.
    #[test]
    fn the_fix_moves_whole_fields_into_their_keys_order() {
        let main = r#"
#![blaze::canonical_cbor]
#[derive(serde::Serialize)]
pub struct Bug {
    /// What went wrong.
    pub what: u64,
    #[serde(skip)]
    pub cache: u32,
    /// The site.
    #[serde(rename = "at")]
    pub site: u64,
    pub kind: u64,
}
"#;
        let want = r#"
#![blaze::canonical_cbor]
#[derive(serde::Serialize)]
pub struct Bug {
    /// The site.
    #[serde(rename = "at")]
    pub site: u64,
    #[serde(skip)]
    pub cache: u32,
    pub kind: u64,
    /// What went wrong.
    pub what: u64,
}
"#;
        let mut fx = fixture::db(&fixture_of(main));
        give_derives(&mut fx);
        let file = *fx.files().last().expect("src/lib.rs");
        let mut db = fx.into_db();
        let krate = blaze_query::db::attach(&db, || {
            hir::Crate::all(&db)
                .into_iter()
                .find(|it| it.display_name(&db).is_some_and(|it| it.to_string() == "main"))
                .expect("the fixture has `main`")
        });
        let file = file.file_id(&db);
        let name_of = |it: FileId| (it == file).then(|| "src/lib.rs".to_owned());
        let flags: Vec<String> = FLAGS.split_whitespace().map(str::to_owned).collect();
        let setup = Setup {
            command_line: CommandLine::parse(&flags).expect("the flags read"),
            conf: Conf::default(),
        };
        fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
        let text = db.file_text(file).text(&db).to_string();
        let head = CRATES.rsplit("crate:main deps:serde,blaze_serde\n").next().unwrap_or("");
        assert_eq!(text, format!("{head}{}", want.trim_start_matches('\n')));
        blaze_query::db::attach(&db, || {
            let found = check(&db, krate, &setup, &name_of).expect("the file is named");
            let left: Vec<&str> = found
                .iter()
                .map(|it| it.diagnostic.message.as_str())
                .filter(|it| it.contains(MESSAGE))
                .collect();
            assert!(left.is_empty(), "the fixed file has nothing left for the lint:\n{text}\n{left:#?}");
        });
    }
}
