//! `absolute_position`: an absolute source position kept where a line added above the code
//! moves what is kept (rules `keys-cover-every-input` and `granular-caching`; docs/lints.md
//! `absolute_position`). The one concept beside it, positions read by a query, is
//! `reads_positions`; this lint is the other half, positions held by a value.
//!
//! The rule: a position that reaches code, a cache key or a kept value is item-relative (the
//! item's anchor and an offset or index in it: `blaze_format::SpanRef`, `blaze_format::RelSpan`,
//! an anchor-relative `span::Span`, codegen's `places::Place::Anchored`), and the absolute
//! `file:line:column` is made only where it is printed. A field that holds an absolute one in a
//! value that is hashed or persisted changes with every line added above it: each key, record
//! or identity holding the value moves although nothing the code does changed.
//!
//! Found, by name resolution and types, outside test code and macro output:
//!
//! - (a, b) a field of a struct, or of a variant of an enum, of a crate of blaze's workspace
//!   whose type implements, derived or by hand, `core::hash::Hash`, serde's `Serialize` or
//!   `Deserialize`, or `blaze_format`'s `Record` or `Persist`, where the field's type is, or
//!   holds (through references, tuples and type arguments), an absolute position:
//!   `core::panic::Location`, `text_size::TextRange`, `line_index::LineCol` or `WideLineCol`,
//!   `hir_expand`'s `FileRange` or `FilePosition`;
//! - (c) a field anywhere in the workspace that holds a `core::panic::Location` by value (a
//!   copy of the record the link placed): it goes stale when a live patch rewrites the record
//!   and moves with the code when hashed. In-process the site is `blaze_core::site::Site`.
//!
//! Not found: `TextSize`, which is a length or an offset into any text (a docs string, a
//! buffer) as often as into a file; a bare `u32` line, which no type tells from a count; the
//! text layer, whose ranges are its own coordinates (`text_size`, `line_index`, `rowan`,
//! `syntax`, `syntax_tree`, `parser`, `span`, `tt`, `mbe`, the `ide` crates, `blaze_fmt`); the
//! position types' own definitions; a `&'static Location` in a value that is neither hashed nor
//! persisted, which is the link's record by reference (what `Site` holds).
//!
//! There is no fix: no item-relative type is a pure substitution, since each needs the item's
//! anchor, which the code that fills the field must supply.

use base_db::Db;
use hir::HirDisplay as _;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Lint, facts, general_facts};
use crate::check::Cx;

const LINT: Lint = Lint::AbsolutePosition;

/// The crates of the text layer, whose ranges are their own coordinates.
const TEXT_LAYER: &[&str] = &[
    "text_size",
    "line_index",
    "rowan",
    "syntax",
    "syntax_tree",
    "parser",
    "span",
    "tt",
    "mbe",
    "blaze_fmt",
];

/// The traits that make a value a key, an identity or a record, by crate and name.
const KEPT: &[(&[&str], &[&str])] = &[
    (&["core", "std"], &["Hash"]),
    (&["serde", "serde_core", "blaze_serde"], &["Serialize", "Deserialize"]),
    (&["blaze_format"], &["Record", "Persist"]),
];

/// An absolute position type a field holds, and what to hold in its place.
#[derive(Clone, Copy)]
struct Absolute {
    name: &'static str,
    instead: &'static str,
    /// A `core::panic::Location` by value, not by reference.
    copied: bool,
}

/// The absolute position `adt` is, if it is one.
fn absolute(db: &Db, adt: hir::Adt) -> Option<&'static str> {
    if general_facts::std_adt(db, adt, "panic", &["Location"]) {
        return Some("core::panic::Location");
    }
    let named = |krate: &str, names: &[&'static str]| -> Option<&'static str> {
        let found = adt.name(db);
        let name = names.iter().copied().find(|it| *it == found.as_str())?;
        facts::crate_is(db, adt.module(db).krate(db), krate).then_some(name)
    };
    named("text_size", &["TextRange"])
        .or_else(|| named("line_index", &["LineCol", "WideLineCol"]))
        .or_else(|| named("hir_expand", &["FileRangeWrapper", "FilePositionWrapper"]))
}

/// What a field of type `ty` holds that is absolute: itself, or through references, tuples,
/// arrays and type arguments, at most `depth` deep.
fn held(db: &Db, ty: &hir::Type<'_>, depth: u32) -> Option<Absolute> {
    let by_value = !ty.is_reference();
    let ty = ty.strip_references();
    if let Some(adt) = ty.as_adt() {
        if let Some(name) = absolute(db, adt) {
            let location = name == "core::panic::Location";
            let instead = match name {
                "core::panic::Location" => {
                    "`blaze_core::site::Site` in the process (the link's record by reference), `blaze_format::SpanRef` (the item's anchor and the span's index) in a record or key"
                }
                "TextRange" | "FileRangeWrapper" | "FilePositionWrapper" => {
                    "a range from the item's anchor (`blaze_format::RelSpan`, or an anchor-relative `span::Span`) with the item by key"
                }
                _ => {
                    "a row and column from the item's start (as codegen's `places::Place::Anchored`), with the item by key"
                }
            };
            return Some(Absolute { name, instead, copied: location && by_value });
        }
    }
    if depth == 0 {
        return None;
    }
    if let Some((element, _)) = ty.as_array(db) {
        return held(db, &element, depth - 1);
    }
    let found = ty.tuple_fields(db).into_iter().find_map(|it| held(db, &it, depth - 1));
    found.or_else(|| ty.type_arguments().find_map(|it| held(db, &it, depth - 1)))
}

/// The first trait of [`KEPT`] `adt` implements, derived or by hand, by its name.
fn kept(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<hir::Name> {
    let db = cx.db();
    hir::Impl::all_for_type(db, cx.krate(), adt.ty(db)).into_iter().find_map(|imp| {
        let trait_ = imp.trait_(db)?;
        let krate = trait_.module(db).krate(db);
        let name = trait_.name(db);
        let known = KEPT.iter().any(|(crates, names)| {
            names.contains(&name.as_str())
                && crates.iter().any(|it| facts::crate_is(db, krate, it))
        });
        known.then_some(name)
    })
}

/// The node of field `name`, the `index`th, of `list`.
fn field_in<'t>(
    list: Option<ast::FieldList<'t>>,
    name: &hir::Name,
    index: usize,
) -> Option<ast::Node<'t>> {
    match list? {
        ast::FieldList::RecordFieldList(list) => list
            .fields()
            .find(|it| it.name().is_some_and(|n| n.text() == name.as_str()))
            .map(|it| it.syntax()),
        ast::FieldList::TupleFieldList(list) => list.fields().nth(index).map(|it| it.syntax()),
    }
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Adt<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.in_file()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let db = cx.db();
    let in_text_layer = cx.krate().display_name(db).is_some_and(|it| {
        let name = it.crate_name().symbol().as_str();
        TEXT_LAYER.contains(&name) || name == "ide" || name.starts_with("ide_")
    });
    if in_text_layer {
        return;
    }
    let Some(adt) = cx.adt_def(item) else { return };
    if absolute(db, adt).is_some() {
        return;
    }
    // Each field with its node: a struct's, or each variant's of an enum.
    // alloc: the fields of one type, a few
    let mut fields: Vec<(String, hir::Field, ast::Node<'t>)> = Vec::new();
    match (adt, item) {
        (hir::Adt::Struct(def), ast::Adt::Struct(node)) => {
            for (index, field) in def.fields(db).into_iter().enumerate() {
                let name = field.name(db);
                if let Some(at) = field_in(node.field_list(), &name, index) {
                    // alloc: a message's part
                    fields.push((name.as_str().to_owned(), field, at));
                }
            }
        }
        (hir::Adt::Enum(def), ast::Adt::Enum(node)) => {
            let Some(list) = node.variant_list() else { return };
            for variant in def.variants(db) {
                let vname = variant.name(db);
                let named = |it: &ast::Variant<'t>| {
                    it.name().is_some_and(|n| n.text() == vname.as_str())
                };
                let Some(written) = list.variants().find(named) else { continue };
                for (index, field) in variant.fields(db).into_iter().enumerate() {
                    let name = field.name(db);
                    if let Some(at) = field_in(written.field_list(), &name, index) {
                        // alloc: a message's part
                        let said = format!("{}::{}", vname.as_str(), name.as_str());
                        fields.push((said, field, at));
                    }
                }
            }
        }
        _ => return,
    }
    // alloc: the fields of one type that hold an absolute position, a few
    let absolutes: Vec<_> = fields
        .into_iter()
        .filter_map(|(name, field, at)| {
            let ty = field.ty(db);
            held(db, &ty, 4).map(|absolute| (name, ty, at, absolute))
        })
        .collect();
    if absolutes.is_empty() {
        return;
    }
    // Its impls are looked up only for a type that holds a position.
    let keeps = kept(cx, adt);
    // alloc: the findings of one type, reported after the walk of its fields
    let mut found = Vec::new();
    for (name, ty, at, absolute) in absolutes {
        let what = match (&keeps, absolute.copied) {
            (Some(trait_), _) => {
                // alloc: a message's part
                format!("which is a key or a record (it implements `{}`)", trait_.as_str())
            }
            (None, true) => {
                // alloc: a message's part
                "a copy of the link's record, which a live patch does not rewrite".to_owned()
            }
            (None, false) => continue,
        };
        found.push((name, ty, at, absolute, what));
    }
    let target = cx.krate().to_display_target(db);
    let type_name = adt.name(db);
    for (name, ty, at, absolute, what) in found {
        let Some(range) = cx.range(at) else { continue };
        let message = format!(
            "the field `{name}` of `{}`, {what}, holds `{}`, an absolute position (`{}`): a line added above the code it is of changes it; hold it item-relative: {}",
            type_name.as_str(),
            ty.display(db, target),
            absolute.name,
            absolute.instead,
        );
        let span = cx.span(range);
        cx.blaze(LINT, at, span, message)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "an absolute position";

    /// `text_size` and `line_index` by the names and crates the real ones have, and a
    /// `blaze_format` with its `Record` trait.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, hash, derive, panic_location
//- /text_size/lib.rs crate:text_size
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TextRange { pub start: u32, pub end: u32 }
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TextSize(pub u32);
//- /line_index/lib.rs crate:line_index
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LineCol { pub line: u32, pub col: u32 }
//- /blaze_format/lib.rs crate:blaze_format
pub unsafe trait Record {}
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpanRef { pub anchor: u64, pub index: u32 }
"#;

    fn found(main: &str) -> String {
        let fixture = format!(
            "{}//- /src/lib.rs crate:blaze_cache deps:text_size,line_index,blaze_format\n{}",
            CRATES.trim_start_matches('\n'),
            main.trim_start_matches('\n')
        );
        lint(&fixture, "-A clippy::all --warn=blaze::absolute_position")
    }

    /// The bug in miniature: a cache key that derives `Hash` over a file range, a record that
    /// implements `Record` by hand over a line and column, an enum variant of a hashed error
    /// that holds a range in an `Option`, and a struct that keeps a `Location` by value.
    #[test]
    fn absolute_positions_in_keys_and_records_are_found() {
        let out = found(
            r#"
use text_size::TextRange;
use line_index::LineCol;
#[derive(Hash, PartialEq, Eq)]
pub struct Key { pub item: u64, pub at: TextRange }
pub struct Row { pub at: LineCol }
unsafe impl blaze_format::Record for Row {}
#[derive(Hash)]
pub enum Refused { Syntax, Unclosed { open: Option<TextRange> } }
pub struct Owed { pub made: core::panic::Location<'static> }
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 4, "{out}");
        assert!(out.contains("the field `at` of `Key`, which is a key or a record (it implements `Hash`)"), "{out}");
        assert!(out.contains("the field `at` of `Row`, which is a key or a record (it implements `Record`)"), "{out}");
        assert!(out.contains("the field `Unclosed::open` of `Refused`"), "{out}");
        assert!(out.contains("the field `made` of `Owed`, a copy of the link's record"), "{out}");
        assert!(out.contains("blaze_core::site::Site"), "{out}");
        assert!(out.contains("blaze_format::RelSpan"), "{out}");
    }

    /// The fix and what is not found: an item-relative key, a `TextSize` (a length as often as
    /// an offset), a range in a value that is neither hashed nor kept, a `&'static Location`
    /// by reference in one, and a test's key.
    #[test]
    fn item_relative_positions_and_unkept_values_are_left_alone() {
        let out = found(
            r#"
use text_size::{TextRange, TextSize};
#[derive(Hash, PartialEq, Eq)]
pub struct Key { pub item: u64, pub at: blaze_format::SpanRef, pub len: TextSize }
pub struct Shown { pub at: TextRange }
pub struct Held { pub site: &'static core::panic::Location<'static> }
#[cfg(test)]
mod tests {
    #[derive(Hash)]
    pub struct Key { pub at: text_size::TextRange }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// An `#[expect]` on the type leaves no output and no unfulfilled expectation; the text
    /// layer is not linted.
    #[test]
    fn an_expect_and_the_text_layer() {
        let out = found(
            r#"
#[expect(blaze::absolute_position, reason = "rendered by the diagnostic, kept only for a crate that does not build")]
#[derive(Hash)]
pub enum Refused { Unclosed { open: text_size::TextRange } }
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
        let fixture = format!(
            "{}//- /src/lib.rs crate:syntax deps:text_size\n#[derive(Hash)]\npub struct SyntaxError(pub u32, pub text_size::TextRange);\n",
            CRATES.trim_start_matches('\n'),
        );
        let out = lint(&fixture, "-A clippy::all --warn=blaze::absolute_position");
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }
}
