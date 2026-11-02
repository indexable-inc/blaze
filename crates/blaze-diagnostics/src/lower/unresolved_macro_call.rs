//! A macro call, attribute or derive whose macro is not found: "cannot find macro `x` in this
//! scope" ("attribute", "derive macro"), with rustc's notes: a macro with a similar name, one
//! that appears later, one of another kind, the name being something else in scope; and for a
//! path of more than one segment, E0433 at the segment not found.
//!
//! Follows rustc's `compiler/rustc_resolve/src/macros.rs` (`finalize_macro_resolutions`),
//! `diagnostics/impls.rs` (`unresolved_macro_suggestions`, `add_typo_suggestion`) and
//! `diagnostics/mod.rs` (`CannotFindIdentInThisScope`, `MacroDefinedLater`, `MacroRulesNot`,
//! `IdentInScopeButItIsDesc`, `ExplicitUnsafeTraits`, `MaybeMissingMacroRulesName`,
//! `AddedMacroUse`, `ConsiderAddingADerive`) (MIT or Apache-2.0).

use base_db::Db;
use either::Either;
use hir::{InFile, ModuleDef, ScopeDef};
use hir_expand::inert_attr_macro::INERT_ATTRIBUTES;
use stdx::edit_distance::find_best_match_for_name;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::unresolved_import::{ROOT, Segment, macro_path_error, path_segments};
use crate::{
    cx::{Cx, item_range_view as item_range, signature_range_view as signature_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

/// The kind of macro a call asks for.
pub(super) use crate::model::MacroKind as Kind;

/// rustc's `MacroKind` of `macro_`.
pub(super) fn kind_of(macro_: hir::Macro, db: &Db) -> Kind {
    match macro_.kind(db) {
        hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn => Kind::Attr,
        hir::MacroKind::Derive | hir::MacroKind::DeriveBuiltIn => Kind::Derive,
        hir::MacroKind::Declarative | hir::MacroKind::DeclarativeBuiltIn | hir::MacroKind::ProcMacro => Kind::Bang,
    }
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedMacroCall) -> Vec<Found> {
    let file = d.range.file_id;
    let root = cx.root(file);
    let attr = (!d.is_bang)
        .then(|| find_node_at_range::<ast::Attr<'_>>(root, d.range.value))
        .flatten()
        .filter(|it| it.syntax().text_range() == d.range.value);
    let kind = match (d.is_bang, attr) {
        (true, _) => Kind::Bang,
        (false, Some(_)) => Kind::Attr,
        (false, None) => Kind::Derive,
    };
    let (node, segments) = match (kind, attr) {
        // alloc: the error's parts, for an error hir found
        (Kind::Attr, Some(attr)) => (attr.syntax(), attr.path().map_or_else(Vec::new, path_segments)),
        (Kind::Bang, _) => match find_node_at_range::<ast::MacroCall<'_>>(root, d.range.value) {
            // alloc: the error's parts, for an error hir found
            Some(call) => (call.syntax(), call.path().map_or_else(Vec::new, path_segments)),
            // alloc: the error's parts, for an error hir found
            None => (covering_node(root, d.range.value), Vec::new()),
        },
        _ => (covering_node(root, d.range.value), derive_segments(root, d.range.value)),
    };
    let Some(ident) = segments.last().cloned() else {
        // alloc: the error's text, for an error hir found
        let name = d.path.segments().last().map_or_else(String::new, |it| it.as_str().to_owned());
        // alloc: the error's text, for an error hir found
        let message = format!("cannot find {} `{name}` in this scope", kind.expected());
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(Diag::error(None, message, cx.span(d.range)), Phase::Expand)];
    };
    if segments.len() > 1 {
        let range = TextRange::new(segments[0].range.start(), ident.range.end());
        let diag = macro_path_error(cx, file, node, &segments, range, (kind.article(), kind.descr()), d.is_bang);
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Expand)];
    }
    let span = cx.span(InFile::new(file, ident.range));
    let name = ident.name.as_str();
    // alloc: the error's text, for an error hir found
    let mut diag = Diag::error(None, format!("cannot find {} `{name}` in this scope", kind.expected()), span.clone());
    let names = names_in_scope(cx, node);
    if let Some(module) = cx.vsema.scope(node).map(|it| it.module()) {
        // alloc: the error's text, for an error hir found
        diag.fact(cx.resolves_fact(module, name), format!("what `{name}` names in this scope"));
    }
    suggestions(cx, &mut diag, kind, &ident, span, &names, node, attr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Expand)]
}

/// rustc's `unresolved_macro_suggestions`, as far as its steps can be told from hir.
#[allow(clippy::too_many_arguments)]
fn suggestions<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    kind: Kind,
    ident: &Segment,
    span: Span,
    names: &[(hir::Name, ScopeDef<'_>)],
    node: ast::Node<'db>,
    attr: Option<ast::Attr<'db>>,
) {
    let db = cx.db;
    let name = ident.name.as_str();
    // clone: on the error path, for an error hir found
    typo(cx, diag, kind, name, span.clone(), names);
    if kind == Kind::Bang && name == "macro_rules" {
        let at = span.shrink_to_hi();
        // alloc: the error's text, for an error hir found
        let spans = MultiSpan { primary: vec![at.clone()], labels: vec![(at, "put a macro name here".to_owned())] };
        diag.span_note(spans, "maybe you have forgotten to define a name for this `macro_rules!`");
        return;
    }
    if kind == Kind::Derive && matches!(name, "Send" | "Sync") {
        // alloc: the error's text, for an error hir found
        diag.span_note(span, format!("unsafe traits like `{name}` should be implemented explicitly"));
        return;
    }
    let local = local_macro_rules(cx);
    let current = cx.vsema.scope(node).map(|it| it.module().nearest_non_block_module(db));
    let call_at = node.text_range().start();
    let unused = local.iter().find(|(it, _, _)| {
        it.name(db).as_str() == name && (kind_of(*it, db) != kind || is_after(cx, *it, node, call_at))
    });
    if let Some(&(macro_, name_file, name_range)) = unused {
        let at = cx.span(InFile::new(name_file, name_range));
        if kind_of(macro_, db) != kind {
            let label = match kind {
                // alloc: the error's text, for an error hir found
                Kind::Bang => format!("`{name}` exists, but has no rules for function-like invocation"),
                // alloc: the error's text, for an error hir found
                Kind::Attr => format!("`{name}` exists, but has no `attr` rules"),
                // alloc: the error's text, for an error hir found
                Kind::Derive => format!("`{name}` exists, but has no `derive` rules"),
            };
            diag.span_label(at, label);
            return;
        }
        if Some(macro_.module(db)) == current {
            diag.span_note(at, "a macro with the same name exists, but it appears later");
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("consider moving the definition of `{name}` before this call"));
            return;
        }
    }
    if kind == Kind::Attr
        && name == "default"
        && let Some(variant) = attr.and_then(|it| it.syntax().parent()).and_then(ast::Variant::cast)
        && let Some(enum_) = variant.syntax().ancestors().find_map(ast::Enum::cast)
    {
        let file = cx.vsema.hir_file_for(enum_.syntax());
        let at = item_range(enum_.syntax()).start();
        diag.suggestion(
            cx.span(InFile::new(file, TextRange::empty(at))),
            "consider adding a derive",
            "#[derive(Default)]\n",
            Applicability::MaybeIncorrect,
        );
    }
    if let Some(desc) = other_meaning(cx, kind, name, names) {
        // alloc: the error's text, for an error hir found
        diag.note(format!("`{name}` is in scope, but it is {desc}"));
        return;
    }
    if local.iter().any(|(it, _, _)| it.name(db).as_str() == name) {
        diag.help("have you added the `#[macro_use]` on the module/import?");
    }
}

/// rustc's typo suggestion of a macro: one of the kind asked for in scope (or a built-in
/// attribute, for an attribute) whose name is close, labelled where it is defined.
fn typo(cx: &Cx<'_, '_>, diag: &mut Diag, kind: Kind, name: &str, span: Span, names: &[(hir::Name, ScopeDef<'_>)]) {
    let db = cx.db;
    let mut candidates: Vec<(&str, Option<hir::Macro>)> = names
        .iter()
        .filter_map(|(it, def)| match def {
            ScopeDef::ModuleDef(ModuleDef::Macro(m)) if kind_of(*m, db) == kind => Some((it.as_str(), Some(*m))),
            _ => None,
        })
        // alloc: the error's parts, for an error hir found
        .collect();
    if kind == Kind::Attr {
        candidates.extend(INERT_ATTRIBUTES.iter().map(|it| (it.name, None)));
    }
    // alloc: the error's parts, for an error hir found
    let words: Vec<&str> = candidates.iter().map(|(it, _)| *it).collect();
    let Some(best) = find_best_match_for_name(&words, name, None).copied() else { return };
    if best == "_" {
        return;
    }
    let found = candidates.iter().find(|(it, _)| *it == best).and_then(|(_, it)| *it);
    let (article, descr) = match found {
        Some(m) => {
            let kind = kind_of(m, db);
            if let Some(at) = head_span(cx, m) {
                // alloc: the error's text, for an error hir found
                diag.span_label(at, format!("similarly named {} `{best}` defined here", kind.descr()));
            }
            (kind.article(), kind.descr())
        }
        None => ("a", "built-in attribute"),
    };
    diag.suggestion_verbose(
        span,
        // alloc: the error's text, for an error hir found
        format!("{article} {descr} with a similar name exists"),
        best,
        Applicability::MaybeIncorrect,
    );
}

/// What `name` is in scope when it is not a macro of `kind`, as rustc's note says it.
fn other_meaning(cx: &Cx<'_, '_>, kind: Kind, name: &str, names: &[(hir::Name, ScopeDef<'_>)]) -> Option<String> {
    let db = cx.db;
    let defs: Vec<ModuleDef> = names
        .iter()
        .filter(|(it, _)| it.as_str() == name)
        .filter_map(|(_, def)| match def {
            ScopeDef::ModuleDef(it) => Some(*it),
            _ => None,
        })
        // alloc: the error's parts, for an error hir found
        .collect();
    let as_kind = |of: Kind| match of {
        // alloc: the error's text, for an error hir found
        Kind::Bang => "a function-like macro".to_owned(),
        // alloc: the error's text, for an error hir found
        Kind::Attr => format!("an attribute: `#[{name}]`"),
        // alloc: the error's text, for an error hir found
        Kind::Derive => format!("a derive macro: `#[derive({name})]`"),
    };
    if let Some(m) = defs.iter().find_map(|it| match it {
        ModuleDef::Macro(m) => Some(*m),
        _ => None,
    }) {
        return Some(as_kind(kind_of(m, db)));
    }
    if kind != Kind::Attr && INERT_ATTRIBUTES.iter().any(|it| it.name == name) {
        return Some(as_kind(Kind::Attr));
    }
    let types = defs.iter().find(|it| {
        matches!(it, ModuleDef::Module(_) | ModuleDef::Adt(_) | ModuleDef::Trait(_) | ModuleDef::TypeAlias(_) | ModuleDef::BuiltinType(_))
    });
    let values = defs.iter().find(|it| matches!(it, ModuleDef::Function(_) | ModuleDef::Const(_) | ModuleDef::Static(_) | ModuleDef::EnumVariant(_)));
    let def = types.or(values)?;
    if kind == Kind::Derive && matches!(def, ModuleDef::Trait(_)) {
        // alloc: the error's text, for an error hir found
        return Some("only a trait, without a derive macro".to_owned());
    }
    let (article, descr) = def_descr(*def);
    // alloc: the error's text, for an error hir found
    Some(format!("{article} {descr}, not {} {}", kind.article(), kind.expected()))
}

/// rustc's `Res::article` and `Res::descr` of an item.
fn def_descr(def: ModuleDef) -> (&'static str, &'static str) {
    match def {
        ModuleDef::Module(_) => ("a", "module"),
        ModuleDef::Adt(hir::Adt::Struct(_)) => ("a", "struct"),
        ModuleDef::Adt(hir::Adt::Enum(_)) => ("an", "enum"),
        ModuleDef::Adt(hir::Adt::Union(_)) => ("a", "union"),
        ModuleDef::Trait(_) => ("a", "trait"),
        ModuleDef::TypeAlias(_) => ("a", "type alias"),
        ModuleDef::BuiltinType(_) => ("a", "builtin type"),
        ModuleDef::Function(_) => ("a", "function"),
        ModuleDef::Const(_) => ("a", "constant"),
        ModuleDef::Static(_) => ("a", "static"),
        ModuleDef::EnumVariant(_) => ("a", "variant"),
        ModuleDef::Macro(_) => ("a", "macro"),
    }
}

/// The names in scope at `node`, with what each means.
fn names_in_scope<'db>(cx: &Cx<'_, 'db>, node: ast::Node<'db>) -> Vec<(hir::Name, ScopeDef<'db>)> {
    // alloc: the error's parts, for an error hir found
    let mut names = Vec::new();
    if let Some(scope) = cx.vsema.scope(node) {
        scope.process_all_names(&mut |name, def| names.push((name, def)));
    }
    names
}

/// The crate's own `macro_rules!` of its modules, each with where its name is.
fn local_macro_rules(cx: &Cx<'_, '_>) -> Vec<(hir::Macro, hir::HirFileId, TextRange)> {
    let db = cx.db;
    // alloc: the error's parts, for an error hir found
    let mut out: Vec<(hir::Macro, hir::HirFileId, TextRange)> = Vec::new();
    for module in cx.krate.modules_iter(db) {
        let declared = module.declarations_iter(db).filter_map(|it| match it {
            ModuleDef::Macro(m) => Some(m),
            _ => None,
        });
        for macro_ in module.legacy_macros_iter(db).chain(declared) {
            if macro_.module(db).krate(db) != cx.krate || out.iter().any(|(it, _, _)| *it == macro_) {
                continue;
            }
            let Some(src) = cx.vsema.source(macro_) else { continue };
            let Either::Left(ast::Macro::MacroRules(rules)) = src.value else { continue };
            let Some(name) = rules.name() else { continue };
            out.push((macro_, src.file_id, name.syntax().text_range()));
        }
    }
    out
}

/// Whether `macro_` is defined after `at` in the file of `node`.
fn is_after<'db>(cx: &Cx<'_, 'db>, macro_: hir::Macro, node: ast::Node<'db>, at: TextSize) -> bool {
    let Some(src) = cx.vsema.source(macro_) else { return false };
    let call_file = cx.vsema.hir_file_for(node);
    let start = src.value.syntax().text_range().start();
    src.file_id == call_file && start > at
}

/// rustc's `guess_head_span` of a macro's `def_span`: a `macro_rules!` up to its `{`, a proc
/// macro's signature.
pub(super) fn head_span(cx: &Cx<'_, '_>, macro_: hir::Macro) -> Option<Span> {
    let src = cx.vsema.source(macro_)?;
    let range = match src.value {
        Either::Left(it) => {
            let node = it.syntax();
            let range = item_range(node);
            let from = usize::from(range.start() - node.text_range().start());
            let head = &node.text()[from..];
            let head = head.find('{').map_or(head, |end| &head[..end]).trim_end();
            TextRange::at(range.start(), TextSize::of(head))
        }
        Either::Right(it) => signature_range(it),
    };
    Some(cx.span(InFile::new(src.file_id, range)))
}

/// The innermost `N` holding `range`: `syntax::algo::find_node_at_range` on a view.
fn find_node_at_range<'t, N: ast::AstNode<'t>>(root: ast::Node<'t>, range: TextRange) -> Option<N> {
    root.covering_element(range).ancestors().find_map(N::cast)
}

/// The smallest node covering `range`.
fn covering_node(root: ast::Node<'_>, range: TextRange) -> ast::Node<'_> {
    match root.covering_element(range) {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => it.parent().unwrap_or(root),
    }
}

/// The segments of a derive's path, which is tokens of the attribute's token tree.
fn derive_segments(root: ast::Node<'_>, range: TextRange) -> Vec<Segment> {
    // alloc: the error's parts, for an error hir found
    let mut segments = Vec::new();
    let mut push = |i: usize, token: ast::Token<'_>| match token.kind() {
        // alloc: the error's text, for an error hir found
        SyntaxKind::COLON2 if i == 0 => segments.push(Segment { name: ROOT.to_owned(), range: token.text_range() }),
        kind if kind == SyntaxKind::IDENT || kind.is_keyword(span::Edition::LATEST) => {
            // alloc: the error's text, for an error hir found
            segments.push(Segment { name: token.text().to_owned(), range: token.text_range() });
        }
        _ => {}
    };
    match root.covering_element(range) {
        NodeOrToken::Token(it) => push(0, it),
        NodeOrToken::Node(it) => it
            .descendants_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .filter(|it| range.contains_range(it.text_range()))
            .enumerate()
            .for_each(|(i, it)| push(i, it)),
    }
    segments
}

#[cfg(test)]
mod tests {
    use span::Edition;
    use syntax::{SourceFile, TextSize};

    use super::*;

    /// The attribute and the macro call around each range, innermost first, of the tree's
    /// nodes: none where the range is outside every one.
    #[test]
    fn the_node_at_a_range_is_the_innermost_holding_it() {
        let text = "#[derive(Debug, ::serde::Serialize)]\n#[inline]\nfn f() { m!(1); a::b!(); }\n\
                    #[cfg_attr(x, derive(Clone))]\nenum E { #[default] A }\n";
        let parse = SourceFile::parse(text, Edition::CURRENT);
        let root = parse.root();
        let at = |written: &str, skip: usize| {
            let start = text.find(written).expect("in the text") + skip;
            TextRange::at(TextSize::from(u32::try_from(start).unwrap()), TextSize::of(&written[skip..]))
        };
        let derive = "#[derive(Debug, ::serde::Serialize)]";
        let cfg_attr = "#[cfg_attr(x, derive(Clone))]";
        for (range, attr, call) in [
            (at("Debug", 0), Some(derive), None),
            (at("::serde::Serialize", 0), Some(derive), None),
            (at("Serialize", 0), Some(derive), None),
            (at("inline", 0), Some("#[inline]"), None),
            (at("1", 0), None, Some("m!(1)")),
            (at("b!", 0), None, Some("a::b!()")),
            (at("x", 0), Some(cfg_attr), None),
            (at("Clone", 0), Some(cfg_attr), None),
            (at("default", 0), Some("#[default]"), None),
            (at("] A", 2), None, None),
            (at("fn f()", 0), None, None),
            (root.text_range(), None, None),
        ] {
            let found_attr = find_node_at_range::<ast::Attr<'_>>(root, range).map(|it| &text[it.syntax().text_range()]);
            let found_call = find_node_at_range::<ast::MacroCall<'_>>(root, range)
                .map(|it| text[it.syntax().text_range()].trim_end_matches(';'));
            assert_eq!((found_attr, found_call), (attr, call), "at {:?}", &text[range]);
        }
    }
}
