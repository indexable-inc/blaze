//! What rustc checks of definitions that the front end reports nothing of: a name defined twice
//! in a namespace of a module, block, enum or trait (E0428 and its kin E0252, E0254, E0255,
//! E0259, E0260); a field declared twice (E0124); a discriminant assigned twice (E0081) and the
//! `repr` of an enum with no variants (E0084); a type of infinite size (E0072); a type or
//! lifetime parameter of a type never used (E0392), or of a type alias (E0091); a generic
//! parameter declared twice (E0403), named `'static` (E0262) or named by an earlier default
//! (E0128); a name bound twice in a parameter list (E0415) or a pattern (E0416).
//!
//! Follows rustc's `compiler/rustc_resolve/src/build_reduced_graph.rs` and
//! `diagnostics/impls.rs` (`report_conflict`), `late.rs` (`with_generic_param_rib`,
//! `visit_generic_params`, `fresh_binding`), `compiler/rustc_hir_analysis/src/collect.rs`
//! (`FieldUniquenessCheckContext`), `check/check.rs` (`check_item_type`, `check_enum`,
//! `detect_discriminant_duplicate`, `check_type_alias_type_params_are_used`), `check/wfcheck.rs` (`check_variances_for_type_defn`,
//! `report_bivariance`), `compiler/rustc_ty_utils/src/representability.rs` and
//! `compiler/rustc_query_impl/src/handle_cycle_error.rs` (`recursive_type_error`)
//! (MIT or Apache-2.0).

mod bindings;
mod conflicts;
mod discriminants;
mod fields;
mod generics;
mod packed;
mod recursive;
mod unused;

use base_db::Db;
use blaze_items::ItemId;
use hir::InFile;
use hir_def::{
    expr_store::Body,
    nameres::demand::every_body,
};
use syntax::{TextRange, TextSize, view};

use crate::{
    cx::{Cx, item_range_view},
    lower::Found,
    model::Span,
};

/// What rustc says of the definitions of `cx.krate` that `hir` does not.
/// `scopes` is [`scopes`], made once per check.
pub(crate) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    let mut found = conflicts::check(cx, scopes);
    found.extend(generics::check(cx, scopes));
    found.extend(bindings::check(cx));
    let adts = adts(cx, scopes);
    found.extend(fields::check(cx, &adts));
    let mut infinite = recursive::Cycles::default();
    for &adt in &adts {
        if let hir::Adt::Enum(it) = adt {
            found.extend(discriminants::check(cx, it));
        }
        found.extend(packed::check(cx, adt));
        found.extend(unused::check(cx, adt));
        found.extend(infinite.check(cx, adt));
    }
    for def in scopes.iter().flat_map(|module| module.declarations_iter(cx.db)) {
        if let hir::ModuleDef::TypeAlias(alias) = def {
            found.extend(unused::check_alias(cx, alias));
        }
    }
    found
}

/// The scopes items are defined in, in rustc's order: the modules of the crate (crate root
/// first, each module before those in it), then the blocks of the bodies that hold items, each
/// with the modules written in it.
pub(crate) fn scopes(cx: &Cx<'_, '_>) -> Vec<hir::Module> {
    let mut scopes = crate::checks::modules(cx);
    for owner in every_body(cx.db, cx.krate.base()) {
        let body = Body::of(cx.db, owner);
        for (_, def_map) in body.blocks(cx.db) {
            scopes.extend(def_map.modules(cx.db).map(|(id, _)| hir::Module::from(id)));
        }
    }
    scopes
}

/// Whether `module` is the scope of a block, rustc's anonymous module.
pub(crate) fn is_block(cx: &Cx<'_, '_>, module: hir::Module) -> bool {
    module.nearest_non_block_module(cx.db) != module
}

/// The structs, enums and unions written in `scopes` (a derive's expansion writes none), in
/// order.
fn adts(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<hir::Adt> {
    scopes
        .iter()
        .flat_map(|module| module.declarations_iter(cx.db))
        .filter_map(|def| match def {
            hir::ModuleDef::Adt(adt) if cx.vsema.source(adt).is_some() => Some(adt),
            _ => None,
        })
        // alloc: setup, once per check
        .collect()
}

/// rustc's `guess_head_span` of an item's span: up to its first `{`, unless that is all of it
/// or spans lines. Read from the text the tree lends, up to the `{`, as [`head_of`] reads a
/// copy.
pub(crate) fn head_range_view(node: view::Node<'_>) -> TextRange {
    let range = item_range_view(node);
    let written = &node.text()[range - node.text_range().start()];
    let head = &written[..written.find('{').unwrap_or(written.len())];
    // The length up to its last character that is not white space, and whether a line ends
    // before that.
    let (mut len, mut at, mut newline, mut multiline) = (TextSize::new(0), TextSize::new(0), false, false);
    for c in head.chars() {
        at += TextSize::of(c);
        if c == '\n' {
            newline = true;
        } else if !c.is_whitespace() {
            multiline |= newline;
            len = at;
        }
    }
    match len > TextSize::new(0) && !multiline {
        true => TextRange::at(range.start(), len),
        false => range,
    }
}

/// rustc's `span_until_char(range, '{')` of the text `text` of a node starting at `base`.
#[cfg(test)]
pub(crate) fn head_of(text: &str, base: TextSize, range: TextRange) -> TextRange {
    let start = usize::from(range.start() - base);
    let end = usize::from(range.end() - base);
    let Some(snippet) = text.get(start..end) else { return range };
    let head = snippet.split('{').next().unwrap_or("").trim_end();
    if !head.is_empty() && !head.contains('\n') {
        TextRange::at(range.start(), TextSize::of(head))
    } else {
        range
    }
}

/// The span of rustc's `guess_head_span` of `node`.
pub(crate) fn head_span(cx: &Cx<'_, '_>, node: InFile<view::Node<'_>>) -> Span {
    cx.span(node.map(head_range_view))
}

/// The item a definition is, for its key in a fact.
pub(crate) fn item_id(db: &Db, def: hir::ModuleDef) -> Option<ItemId> {
    Some(match def {
        hir::ModuleDef::Function(it) => ItemId::Function(it.try_into().ok()?),
        hir::ModuleDef::Adt(it) => ItemId::Adt(it.into()),
        hir::ModuleDef::Trait(it) => ItemId::Trait(it.into()),
        hir::ModuleDef::Const(it) => ItemId::Const(it.into()),
        hir::ModuleDef::Static(it) => ItemId::Static(it.into()),
        hir::ModuleDef::TypeAlias(it) => ItemId::Alias(it.into()),
        hir::ModuleDef::Macro(it) => ItemId::Macro(it.into()),
        hir::ModuleDef::EnumVariant(it) => ItemId::Adt(hir::Adt::from(it.parent_enum(db)).into()),
        hir::ModuleDef::Module(_) | hir::ModuleDef::BuiltinType(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_head_of_an_item_stops_before_its_brace() {
        let text = "struct Foo {\n    x: u8,\n}";
        let range = TextRange::up_to(TextSize::of(text));
        assert_eq!(&text[head_of(text, 0.into(), range)], "struct Foo");
        let text = "struct Bar;";
        let range = TextRange::up_to(TextSize::of(text));
        assert_eq!(&text[head_of(text, 0.into(), range)], "struct Bar;");
        let text = "fn f<T>()\nwhere T: Copy {}";
        let range = TextRange::up_to(TextSize::of(text));
        assert_eq!(&text[head_of(text, 0.into(), range)], text);
    }

    /// The head read from the tokens is the one the text's copy gives, for heads on one line,
    /// over several, with none, after attributes and with a `{` in a comment.
    #[test]
    fn the_head_read_in_place_is_the_copys() {
        use syntax::{
            Edition, SourceFile,
            view::{self as ast, AstNode as _},
        };

        let text = r#"
#[derive(Clone)]
struct One { x: u8 }
struct Two;
enum Three /* { */ { A }
fn four<T>()
where T: Copy {}
trait Five   
{}
impl Five for Two {}
"#;
        let parse = SourceFile::parse(text, Edition::Edition2021);
        let mut compared = 0;
        for node in parse.root().descendants().filter(|it| ast::Item::can_cast(it.kind())) {
            let plain = head_of(node.text(), node.text_range().start(), item_range_view(node));
            assert_eq!(head_range_view(node), plain, "at {node:?}");
            compared += 1;
        }
        assert!(compared >= 6, "the text has six items: {compared}");
    }
}
