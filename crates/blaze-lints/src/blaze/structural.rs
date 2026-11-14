//! The structural lints: code written by hand that the compiler could derive from a type's
//! structure (its fields, its variants), and the bugs that writing it by hand causes. Each is
//! built on one analysis, [`uniform`]: whether a body is the same template once per field.
//! A hand-written `Clone` identical to the derive, an `Eq` that skips a field its `Hash`
//! covers, a variant list that forgot the newest variant, a codec written field by field, a
//! conversion that crosses two fields: each is code whose shape is the type's, kept in step by
//! hand.
//!
//! The end state is blaze's comptime reflection (`for const f in fields_of::<T>()`,
//! `variants_of::<E>()`, docs/v2/plan/design/comptime.md): one generic impl over a type's
//! structure, so a field or variant added later is covered with no edit. While cargo builds
//! blaze's own crates the fix is a `#[derive]` or a declarative table where one exists, and
//! each lint names the reflection form as where it ends.
//!
//! One module per lint under `structural/`; this one walks the file once (the file walk, not a
//! body's) and hands each the items it looks at. Findings are reported through
//! `std_mistakes::report`, the one home of a finding with its fix.

mod codec;
mod conversion;
mod derivable;
mod field_list;
mod misses_field;
#[cfg(test)]
mod tests;
pub(super) mod uniform;
mod variant_list;
mod visitor;

use std::alloc::Global;

use base_db::CrateOrigin;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasGenericParams as _, HasModuleItem as _, HasName as _},
};

use self::uniform::{Analysis, Body, Scope};
use super::{Lint, exempt};
use crate::check::Cx;

/// The tag of the comment that says why a field is left out of a field-wise body on purpose:
/// `let Self { a, b, cache: _ } = self; // left out: cache: rebuilt from a`. The one marker of
/// every lint that asks whether a body covers every field, `key_ignores_field` too.
pub(super) const LEFT_OUT: &str = "left out";

/// The lints of this module.
const LINTS: &[Lint] = &[
    Lint::HandWrittenDerivable,
    Lint::ImplMissesField,
    Lint::HandKeptVariantList,
    Lint::VariantListDrift,
    Lint::HandWrittenCodec,
    Lint::FieldWiseConversion,
    Lint::ConversionMissesField,
    Lint::HandWrittenVisitor,
    Lint::RepeatedFieldList,
    Lint::RegistrySpread,
];

/// The structural lints of the file the walk is at: once per file, from `blaze.rs`'s root
/// pass, never for a body.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>) {
    let root = cx.root();
    if root.kind() != SyntaxKind::SOURCE_FILE
        || cx.analysis().is_some()
        || cx.expansion().is_some()
        || !cx.in_file()
        || !LINTS.iter().any(|it| cx.on_blaze(*it))
    {
        return;
    }
    let Some(file) = ast::SourceFile::cast(root) else { return };
    let items = Items::of(cx, file);
    let impls = impls_of(cx, &items);
    derivable::check(cx, &items, &impls);
    misses_field::check(cx, &items, &impls);
    codec::check(cx, &impls);
    conversion::check(cx, &impls);
    visitor::check(cx, &impls);
    field_list::check(cx, &items, &impls);
    variant_list::check(cx, &items);
}

/// The items of a file the lints look at, outside test code, in the order written: those of
/// the file and of its inline modules (an item inside a function body is not looked at).
pub(super) struct Items<'t> {
    pub(super) structs: Vec<ast::Struct<'t>, Global>,
    pub(super) enums: Vec<ast::Enum<'t>, Global>,
    pub(super) impls: Vec<ast::Impl<'t>, Global>,
    /// `const` and `static` items, of the file, its modules and its impls.
    pub(super) consts: Vec<ast::Node<'t>, Global>,
    pub(super) macros: Vec<ast::MacroRules<'t>, Global>,
}

impl<'t> Items<'t> {
    fn of(cx: &Cx<'_, 't>, file: ast::SourceFile<'t>) -> Items<'t> {
        let mut items = Items {
            structs: Vec::new_in(Global),
            enums: Vec::new_in(Global),
            impls: Vec::new_in(Global),
            consts: Vec::new_in(Global),
            macros: Vec::new_in(Global),
        };
        let mut stack: Vec<ast::Item<'t>, Global> = Vec::new_in(Global);
        stack.extend(file.items());
        stack.reverse();
        while let Some(item) = stack.pop() {
            if cx.is_in_test(item.syntax()) {
                continue;
            }
            match item {
                ast::Item::Struct(it) => items.structs.push(it),
                ast::Item::Enum(it) => items.enums.push(it),
                ast::Item::Impl(it) => {
                    items.impls.push(it);
                    let Some(list) = it.assoc_item_list() else { continue };
                    for assoc in list.assoc_items() {
                        if let ast::AssocItem::Const(c) = assoc {
                            items.consts.push(c.syntax());
                        }
                    }
                }
                ast::Item::Const(it) => items.consts.push(it.syntax()),
                ast::Item::Static(it) => items.consts.push(it.syntax()),
                ast::Item::MacroRules(it) => items.macros.push(it),
                ast::Item::Module(it) => {
                    if let Some(list) = it.item_list() {
                        let mut inner: Vec<ast::Item<'t>, Global> = Vec::new_in(Global);
                        inner.extend(list.items());
                        while let Some(it) = inner.pop() {
                            stack.push(it);
                        }
                    }
                }
                _ => {}
            }
        }
        items
    }

    /// The struct of the file `adt` is, by name resolution.
    pub(super) fn struct_of(&self, cx: &Cx<'_, 't>, adt: hir::Adt) -> Option<ast::Struct<'t>> {
        let name = adt.name(cx.db());
        let name = name.as_str();
        self.structs.iter().copied().find(|it| {
            it.name().is_some_and(|it| it.text() == name)
                && cx.adt_def(ast::Adt::Struct(*it)) == Some(adt)
        })
    }
}

/// A method of an impl, analysed over the impl's type's fields.
pub(super) struct Method<'t> {
    pub(super) node: ast::Fn<'t>,
    pub(super) name: &'t str,
    pub(super) body: Body<'t>,
    pub(super) analysis: Analysis,
    /// The parameters other than `self`, by name.
    pub(super) params: Vec<&'t str, Global>,
}

/// An impl of a struct, with its trait and its fields.
pub(super) struct Impl<'t> {
    pub(super) node: ast::Impl<'t>,
    /// The last segment of the trait's path (`Debug` of `fmt::Debug`).
    pub(super) trait_name: Option<&'t str>,
    /// Whether the trait is the standard library's (`core`, `alloc` or `std`).
    pub(super) std: bool,
    /// The type's name as the impl writes it.
    pub(super) type_name: &'t str,
    pub(super) adt: hir::Adt,
    /// The struct's fields, in declaration order: names, `0`, `1`, ... for a tuple struct.
    pub(super) fields: Vec<String, Global>,
    /// Whether the impl or the struct has generic parameters.
    pub(super) generic: bool,
    pub(super) methods: Vec<Method<'t>, Global>,
}

impl<'t> Impl<'t> {
    pub(super) fn field_names(&self) -> Vec<&str, Global> {
        let mut out: Vec<&str, Global> = Vec::new_in(Global);
        out.extend(self.fields.iter().map(String::as_str));
        out
    }

    pub(super) fn method(&self, name: &str) -> Option<&Method<'t>> {
        self.methods.iter().find(|it| it.name == name)
    }

    /// Whether this is an impl of std's trait `name`.
    pub(super) fn is_std(&self, name: &str) -> bool {
        self.std && self.trait_name == Some(name)
    }

    /// What the impl is written as in a message: `impl Debug for Point`, `impl Point`.
    pub(super) fn said(&self) -> String {
        match self.trait_name {
            Some(name) => format!("impl {name} for {}", self.type_name),
            None => format!("impl {}", self.type_name),
        }
    }
}

/// The impls of the file's structs (the type may be another file's), each method analysed.
fn impls_of<'t>(cx: &Cx<'_, 't>, items: &Items<'t>) -> Vec<Impl<'t>, Global> {
    let db = cx.db();
    let mut out: Vec<Impl<'t>, Global> = Vec::new_in(Global);
    for &node in &items.impls {
        let Some(def) = cx.impl_def(node) else { continue };
        let Some(adt @ hir::Adt::Struct(strukt)) = def.self_ty(db).as_adt() else { continue };
        let Some(type_name) = node.self_ty().and_then(uniform::type_name) else { continue };
        let trait_name = node.trait_().and_then(uniform::type_name);
        let std = def
            .trait_(db)
            .is_some_and(|it| matches!(it.module(db).krate(db).origin(db), CrateOrigin::Lang(_)));
        let mut fields: Vec<String, Global> = Vec::new_in(Global);
        fields.extend(strukt.fields(db).into_iter().map(|it| it.name(db).as_str().to_owned()));
        let struct_generic = items
            .struct_of(cx, adt)
            .is_none_or(|it| it.generic_param_list().is_some());
        let generic = node.generic_param_list().is_some() || struct_generic;
        let mut imp = Impl {
            node,
            trait_name,
            std,
            type_name,
            adt,
            fields,
            generic,
            methods: Vec::new_in(Global),
        };
        imp.methods = methods_of(cx, &imp);
        out.push(imp);
    }
    out
}

/// Each method of `imp` with a body, analysed over its type's fields.
fn methods_of<'t>(cx: &Cx<'_, 't>, imp: &Impl<'t>) -> Vec<Method<'t>, Global> {
    let mut out: Vec<Method<'t>, Global> = Vec::new_in(Global);
    let Some(list) = imp.node.assoc_item_list() else { return out };
    let fields = imp.field_names();
    let makers = ["Self", imp.type_name];
    let text = cx.text();
    let excused = |range: TextRange| exempt::commented(text, range, LEFT_OUT);
    for assoc in list.assoc_items() {
        let ast::AssocItem::Fn(f) = assoc else { continue };
        let (Some(name), Some(block)) = (f.name(), f.body()) else { continue };
        let bases = uniform::bases_of(f, &makers);
        let scope = Scope { fields: &fields, owners: &fields, bases: &bases, makers: &makers };
        let Some(body) = uniform::split(&scope, block, &excused) else { continue };
        let least = if fields.len() == 1 { 1 } else { 2 };
        let analysis = uniform::analyse(&scope, &body, least);
        let params = uniform::param_names(f);
        out.push(Method { node: f, name: name.text(), body, analysis, params });
    }
    out
}

/// The traits a struct's `#[derive(..)]` attributes name, by their last segment.
pub(super) fn derived<'t>(strukt: ast::Struct<'t>) -> Vec<&'t str, Global> {
    let mut out: Vec<&'t str, Global> = Vec::new_in(Global);
    for attr in strukt.attrs() {
        if attr.simple_name() != Some("derive") {
            continue;
        }
        let Some(tree) = attr.meta().and_then(|it| it.syntax().children().find_map(ast::TokenTree::cast))
        else {
            continue;
        };
        let mut last: Option<&'t str> = None;
        for element in tree.syntax().children_with_tokens() {
            let Some(token) = element.into_token() else { continue };
            match token.kind() {
                SyntaxKind::IDENT => last = Some(token.text()),
                SyntaxKind::COMMA | SyntaxKind::R_PAREN => {
                    if let Some(name) = last.take() {
                        out.push(name);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// The names of `fields` at `at`, written for a message: "`a`, `b` and `c`".
pub(super) fn listed(fields: &[&str], at: &[usize]) -> String {
    let mut names: Vec<String, Global> = Vec::new_in(Global);
    names.extend(at.iter().filter_map(|it| fields.get(*it)).map(|it| format!("`{it}`")));
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., tail] => format!("{} and {tail}", init.join(", ")),
    }
}
