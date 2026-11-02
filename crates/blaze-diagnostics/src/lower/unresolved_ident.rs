//! E0425: a name that resolves to no value where it is used (`x`, `f()`, an implicit capture
//! `{x}` of `format_args!`), with "not found in this scope", the name in scope of a similar
//! spelling, and the items of that name elsewhere to import. E0424 for `self` where there is no
//! `self` parameter, and E0531 for a name a pattern needs.
//!
//! [`typo`], [`imports`] and [`show_imports`] are rustc's for every path that resolves to
//! nothing, in either namespace: `checks::names::paths` gives them for a type or trait.
//! [`imports`] reads the check's [`Decls`](crate::print::paths::Decls), every declaration by
//! name, built once; [`typo`] chooses among the names in scope by their text and finds where
//! only the chosen one is defined.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late/diagnostics.rs`
//! (`smart_resolve_report_errors`, `suggest_self_value`, `suggest_typo`,
//! `lookup_typo_candidate`), `compiler/rustc_resolve/src/late.rs` (`PathSource::descr_expected`,
//! `is_expected`, `error_code`), `compiler/rustc_resolve/src/diagnostics/impls.rs`
//! (`add_typo_suggestion`, `lookup_import_candidates`, `show_candidates`, `UsePlacementFinder`),
//! `compiler/rustc_hir/src/def.rs` (`Res::descr`) and `compiler/rustc_span/src/source_map.rs`
//! (`guess_head_span`) (MIT or Apache-2.0).

use either::Either;
use hir::{HasAttrs as _, InFile, ModuleDef};
use span::Edition;
use stdx::edit_distance::find_best_match_for_name;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, AstToken as _, HasModuleItem as _, HasName as _},
};

use crate::{
    checks::head_range_view as head_range,
    cx::{Cx, item_range_view as item_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedIdent) -> Vec<Found> {
    let (ptr, in_node) = d.node.value;
    let ptr = InFile::new(d.node.file_id, ptr);
    let node = cx.node_view(ptr);
    let base = cx.span_of_ptr(ptr);
    let syntax = node.value.syntax();
    let (span, name) = match in_node {
        Some(range) => {
            let text = syntax.text();
            let name = text.get(usize::from(range.start())..usize::from(range.end())).unwrap_or_default();
            let span = match cx.snippet(&base) == Some(text) {
                true => base.with_lo(base.lo + u32::from(range.start())).with_hi(base.lo + u32::from(range.end())),
                // clone: on the error path, for an error hir found
                false => base.clone(),
            };
            (span, name)
        }
        // clone: on the error path, for an error hir found
        None => (base.clone(), written_name(node.value)),
    };
    let source = PathSource::of(node.value, in_node.is_some(), name);
    let scope = cx.vsema.scope(syntax);
    let module = scope.as_ref().map(|it| it.module());

    let mut diag = if name == "self" {
        // clone: on the error path, for an error hir found
        self_value(cx, syntax, d.node.file_id, source, span.clone())
    } else {
        // alloc: the error's text, for an error hir found
        let message = format!("cannot find {} `{name}` in this scope", source.expected());
        // clone: on the error path, for an error hir found
        let mut diag = Diag::error(Some(source.code()), message, span.clone());
        let suggested =
            scope.as_ref().is_some_and(|scope| typo(cx, &mut diag, Names::Scope(scope), source, name, &span, &[]));
        if !suggested {
            // clone: on the error path, for an error hir found
            diag.span_label(span.clone(), "not found in this scope");
        }
        if let Some(module) = module {
            let found = imports(cx, module, source, name, None);
            show_imports(cx, &mut diag, module, found, false);
        }
        diag
    };
    if let Some(module) = module {
        // alloc: the error's text, for an error hir found
        diag.fact(cx.resolves_fact(module, name), format!("what `{name}` names in its scope"));
    }
    code_fact(cx, &mut diag, InFile::new(d.node.file_id, d.node.value.0.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}

/// The name a path expression or pattern is written with.
fn written_name<'t>(node: Either<ast::Expr<'t>, ast::Pat<'t>>) -> &'t str {
    let path = match node {
        Either::Left(ast::Expr::PathExpr(it)) => it.path(),
        Either::Right(ast::Pat::PathPat(it)) => it.path(),
        Either::Right(ast::Pat::TupleStructPat(it)) => it.path(),
        Either::Right(ast::Pat::IdentPat(it)) => return it.name().map_or("", |it| it.text()),
        Either::Left(it) => return it.syntax().text(),
        Either::Right(it) => return it.syntax().text(),
    };
    match path.and_then(|it| it.segment()) {
        Some(segment) if segment.self_token().is_some() => "self",
        Some(segment) => segment.name_ref().map_or("", |it| it.text()),
        None => "",
    }
}

/// The namespace a path is resolved in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ns {
    Type,
    Value,
}

/// What rustc's resolver looked for a name as: its `PathSource`, of the places blaze reports a
/// name that resolves to nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PathSource {
    /// An expression.
    Value,
    /// The callee of a call; with whether the name starts upper case.
    Callee(bool),
    /// A pattern that is a path.
    Pat,
    /// The path of a tuple struct pattern.
    TupleStruct,
    /// The path of a struct literal or a record pattern.
    Struct,
    /// A type.
    Type,
    /// A trait: a bound, the trait of an impl, `<T as Trait>`.
    Trait,
}

impl PathSource {
    pub(crate) fn of(node: Either<ast::Expr<'_>, ast::Pat<'_>>, captured: bool, name: &str) -> PathSource {
        match node {
            Either::Left(_) if captured => PathSource::Value,
            Either::Left(ast::Expr::RecordExpr(_)) | Either::Right(ast::Pat::RecordPat(_)) => PathSource::Struct,
            Either::Left(expr) => {
                let callee = expr
                    .syntax()
                    .parent()
                    .and_then(ast::CallExpr::cast)
                    .and_then(|call| call.expr())
                    .is_some_and(|callee| callee.syntax() == expr.syntax());
                match callee {
                    true => PathSource::Callee(name.chars().next().is_some_and(char::is_uppercase)),
                    false => PathSource::Value,
                }
            }
            Either::Right(ast::Pat::TupleStructPat(_)) => PathSource::TupleStruct,
            Either::Right(_) => PathSource::Pat,
        }
    }

    pub(crate) fn ns(self) -> Ns {
        match self {
            PathSource::Value | PathSource::Callee(_) | PathSource::Pat | PathSource::TupleStruct => Ns::Value,
            PathSource::Struct | PathSource::Type | PathSource::Trait => Ns::Type,
        }
    }

    /// `descr_expected`.
    pub(crate) fn expected(self) -> &'static str {
        match self {
            PathSource::Value => "value",
            PathSource::Callee(false) => "function",
            PathSource::Callee(true) => "function, tuple struct or tuple variant",
            PathSource::Pat => "unit struct, unit variant or constant",
            PathSource::TupleStruct => "tuple struct or tuple variant",
            PathSource::Struct => "struct, variant or union type",
            PathSource::Type => "type",
            PathSource::Trait => "trait",
        }
    }

    /// `error_code` of a value path with no resolution in the namespace.
    pub(crate) fn code(self) -> &'static str {
        match self {
            PathSource::Pat | PathSource::TupleStruct => "E0531",
            PathSource::Struct => "E0422",
            _ => "E0425",
        }
    }

    /// `is_expected` of a thing rustc's `Res::descr` names `descr`, in the source's namespace.
    pub(crate) fn is_expected(self, descr: &str) -> bool {
        match self {
            PathSource::Value | PathSource::Callee(_) => true,
            PathSource::Pat => matches!(descr, "unit struct" | "unit variant" | "constant"),
            PathSource::TupleStruct => matches!(descr, "tuple struct" | "tuple variant"),
            PathSource::Struct => matches!(descr, "struct" | "variant" | "union" | "type alias" | "self type"),
            PathSource::Type => matches!(
                descr,
                "struct" | "enum" | "union" | "trait" | "type alias" | "builtin type" | "type parameter" | "self type"
            ),
            PathSource::Trait => descr == "trait",
        }
    }
}

/// E0424 (`suggest_self_value`): `self` where no `self` parameter is.
fn self_value(cx: &Cx<'_, '_>, syntax: ast::Node<'_>, file: hir::HirFileId, source: PathSource, span: Span) -> Diag {
    // alloc: the error's text, for an error hir found
    let message = format!("cannot find {} `self` in this scope", source.expected());
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0424"), message, span.clone());
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    if matches!(source, PathSource::Pat | PathSource::TupleStruct) {
        diag.span_label(span, "`self` value is a keyword and may not be bound to variables or shadowed");
        diag.note("a module named `self` exists in another namespace");
        return diag;
    }
    diag.span_label(span, "`self` value is a keyword only available in methods with a `self` parameter");
    if let Some(f) = syntax.ancestors().find_map(ast::Fn::cast) {
        let params = f.param_list();
        if params.is_some_and(|it| it.self_param().is_some()) {
            diag.span_label(
                at(item_range(f.syntax())),
                "this function has a `self` parameter, but a macro invocation can only access identifiers it receives from parameters",
            );
        } else {
            let is_assoc = f.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST);
            let doesnt = match is_assoc {
                true => {
                    let first = params.and_then(|it| it.params().next());
                    let sugg = match (first, params.and_then(|it| it.l_paren_token())) {
                        (Some(first), _) => Some((at(first.syntax().text_range()).shrink_to_lo(), "&self, ")),
                        (None, Some(paren)) => Some((at(paren.text_range()).shrink_to_hi(), "&self")),
                        (None, None) => None,
                    };
                    if let Some((span, code)) = sugg {
                        diag.suggestion_verbose(
                            span,
                            "add a `self` receiver parameter to make the associated `fn` a method",
                            code,
                            Applicability::MaybeIncorrect,
                        );
                    }
                    "doesn't"
                }
                false => "can't",
            };
            if let Some(name) = f.name() {
                // alloc: the error's text, for an error hir found
                diag.span_label(at(name.syntax().text_range()), format!("this function {doesnt} have a `self` parameter"));
            }
        }
    }
    diag.note("a module named `self` exists in another namespace");
    diag
}

/// rustc's `Res::descr` of an item in namespace `ns`: in the value namespace, the constructor
/// of a struct or variant that has one.
pub(crate) fn def_descr(cx: &Cx<'_, '_>, ns: Ns, def: ModuleDef) -> Option<&'static str> {
    if ns == Ns::Type {
        return Some(match def {
            ModuleDef::Module(_) => "module",
            ModuleDef::Function(_) => "function",
            ModuleDef::Adt(hir::Adt::Struct(_)) => "struct",
            ModuleDef::Adt(hir::Adt::Enum(_)) => "enum",
            ModuleDef::Adt(hir::Adt::Union(_)) => "union",
            ModuleDef::EnumVariant(_) => "variant",
            ModuleDef::Const(_) => "constant",
            ModuleDef::Static(_) => "static",
            ModuleDef::Trait(_) => "trait",
            ModuleDef::TypeAlias(_) => "type alias",
            ModuleDef::BuiltinType(_) => "builtin type",
            ModuleDef::Macro(it) => match it.kind(cx.db) {
                hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn => "attribute macro",
                hir::MacroKind::Derive | hir::MacroKind::DeriveBuiltIn => "derive macro",
                _ => "macro",
            },
        });
    }
    Some(match def {
        ModuleDef::Function(_) => "function",
        ModuleDef::Const(_) => "constant",
        ModuleDef::Static(_) => "static",
        ModuleDef::Adt(hir::Adt::Struct(it)) => match it.kind(cx.db) {
            hir::StructKind::Tuple => "tuple struct",
            hir::StructKind::Unit => "unit struct",
            hir::StructKind::Record => return None,
        },
        ModuleDef::EnumVariant(it) => match it.kind(cx.db) {
            hir::StructKind::Tuple => "tuple variant",
            hir::StructKind::Unit => "unit variant",
            hir::StructKind::Record => return None,
        },
        _ => return None,
    })
}

/// The head span of an item's definition, of the kinds a path in `ns` may name.
pub(crate) fn def_span(cx: &Cx<'_, '_>, ns: Ns, def: ModuleDef) -> Option<Span> {
    let source: InFile<ast::Node<'_>> = match (ns, def) {
        (Ns::Value, ModuleDef::Function(it)) => cx.vsema.source(it)?.map(|it| it.syntax()),
        (Ns::Value, ModuleDef::Const(it)) => cx.vsema.source(it)?.map(|it| it.syntax()),
        (Ns::Value, ModuleDef::Static(it)) => cx.vsema.source(it)?.map(|it| it.syntax()),
        (Ns::Value, ModuleDef::EnumVariant(it)) => cx.vsema.source(it)?.map(|it| it.syntax()),
        (Ns::Type, ModuleDef::Module(it)) => it.declaration_source_view(cx.db)?.map(|it| it.syntax()),
        (Ns::Type, ModuleDef::Trait(it)) => cx.vsema.source(it)?.map(|it| it.syntax()),
        (Ns::Type, ModuleDef::TypeAlias(it)) => cx.vsema.source(it)?.map(|it| it.syntax()),
        (_, ModuleDef::Adt(it)) => cx.vsema.source(it)?.map(|it| it.syntax()),
        _ => return None,
    };
    Some(cx.span(source.map(head_range)))
}

/// Where a type parameter is declared: its name.
fn param_span(cx: &Cx<'_, '_>, param: hir::TypeParam) -> Option<Span> {
    let source = cx.vsema.source(param.merge())?;
    let range = match source.value {
        Either::Left(ast::TypeOrConstParam::Type(it)) => it.name()?.syntax().text_range(),
        _ => return None,
    };
    Some(cx.span(source.with_value(range)))
}

/// rustc's `Res::descr` of what a name in scope is, in namespace `ns`.
fn scope_descr(cx: &Cx<'_, '_>, ns: Ns, def: &hir::ScopeDef<'_>) -> Option<&'static str> {
    Some(match (ns, def) {
        (Ns::Value, hir::ScopeDef::Local(_)) => "local variable",
        (Ns::Value, hir::ScopeDef::GenericParam(hir::GenericParam::ConstParam(_))) => "const parameter",
        (Ns::Type, hir::ScopeDef::GenericParam(hir::GenericParam::TypeParam(_))) => "type parameter",
        (Ns::Type, hir::ScopeDef::ImplSelfType(_) | hir::ScopeDef::AdtSelfType(_)) => "self type",
        (_, hir::ScopeDef::ModuleDef(def)) => return def_descr(cx, ns, *def),
        _ => return None,
    })
}

/// Where a name in scope is defined, and whether it is a local's binding: an item's head, a
/// local's name, a type parameter's name.
fn scope_span<'db>(cx: &Cx<'_, 'db>, ns: Ns, def: &hir::ScopeDef<'db>) -> (Option<Span>, bool) {
    match def {
        hir::ScopeDef::Local(local) => {
            let binding =
                local.primary_source_view(cx.db).name().map(|it| cx.span(it.map(|it| it.syntax().text_range())));
            (binding, true)
        }
        hir::ScopeDef::GenericParam(hir::GenericParam::TypeParam(it)) => (param_span(cx, *it), false),
        hir::ScopeDef::ModuleDef(it) => (def_span(cx, ns, *it), false),
        _ => (None, false),
    }
}

/// The names [`typo`] chooses from: those in scope at a place, or those of a module a path's
/// prefix names.
#[derive(Clone, Copy)]
pub(crate) enum Names<'s, 'db> {
    Scope(&'s hir::SemanticsScope<'db>),
    Module(hir::Module),
}

/// rustc's `suggest_typo`: the name of `names`, of a kind the place expects, closest to `name`
/// and not one of `exclude` (what another suggestion already names); whether it suggested one.
pub(crate) fn typo<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    names: Names<'_, 'db>,
    source: PathSource,
    name: &str,
    span: &Span,
    exclude: &[String],
) -> bool {
    let ns = source.ns();
    // alloc: the names in scope of a name that resolves to nothing, once per diagnostic
    let mut candidates: Vec<(hir::Name, &'static str, hir::ScopeDef<'db>)> = Vec::new();
    let mut consider = |found: hir::Name, def: hir::ScopeDef<'db>| {
        if let Some(descr) = scope_descr(cx, ns, &def).filter(|it| source.is_expected(it)) {
            candidates.push((found, descr, def));
        }
    };
    match names {
        Names::Scope(scope) => scope.process_all_names(&mut |found, def| consider(found, def)),
        Names::Module(module) => {
            for (found, def) in module.scope(cx.db, None) {
                consider(found, def);
            }
        }
    }
    candidates.sort_by(|a, b| a.0.symbol().cmp_order(b.0.symbol()));
    // alloc: the candidates' texts, borrowed from the interner
    let texts: Vec<&'static str> = candidates.iter().map(|it| it.0.symbol().as_str()).collect();
    let Some(best) = find_best_match_for_name(&texts, name, None).copied() else { return false };
    if best == name || best == "_" || exclude.iter().any(|it| it == best) {
        return false;
    }
    let Some(&(found, descr, def)) = candidates.iter().find(|it| it.0.symbol().as_str() == best) else {
        return false;
    };
    let (def, is_local) = scope_span(cx, ns, &def);
    let found = found.symbol().as_str();
    if let Some(def) = &def
        && def.file == span.file
        && def.lo < span.hi
        && span.lo < def.hi
    {
        return false;
    }
    let mut labelled = false;
    if !is_local && let Some(def) = &def {
        // alloc: the error's text, for an error hir found
        diag.span_label(def.clone(), format!("similarly named {descr} `{found}` defined here"));
        labelled = true;
    }
    let snippet = cx.snippet(span);
    match (&def, found.strip_prefix('_')) {
        (Some(binding), Some(unused)) if snippet == Some(unused) && is_local => {
            if !labelled {
                // alloc: the error's text, for an error hir found
                diag.span_label(binding.clone(), format!("`{found}` defined here"));
            }
            diag.suggestion_verbose(
                // clone: on the error path, for an error hir found
                binding.clone(),
                // alloc: the error's text, for an error hir found
                format!("the leading underscore in `{found}` marks it as unused, consider renaming it to `{unused}`"),
                unused,
                Applicability::MaybeIncorrect,
            );
        }
        _ => {
            diag.suggestion_verbose(
                // clone: on the error path, for an error hir found
                span.clone(),
                // alloc: the error's text, for an error hir found
                format!("a {descr} with a similar name exists"),
                found,
                Applicability::MaybeIncorrect,
            );
        }
    }
    true
}

/// An item named as the name looked for, of the crate or one it depends on: rustc's
/// `ImportSuggestion`.
pub(crate) struct Import {
    pub(crate) path: String,
    /// Of this crate: rustc lists those first, by their `crate::` path.
    of_crate: bool,
    pub(crate) descr: &'static str,
    def: ModuleDef,
}

/// The items [`imports`] found in namespace `ns`: those to `use`, and those of the crate that
/// exist but are not visible from where the name is.
pub(crate) struct Imports {
    ns: Ns,
    pub(crate) accessible: Vec<Import>,
    pub(crate) inaccessible: Vec<Import>,
}

impl Imports {
    pub(crate) fn is_empty(&self) -> bool {
        self.accessible.is_empty() && self.inaccessible.is_empty()
    }
}

/// rustc's `lookup_import_candidates`: the items named `name` of a kind `source` expects,
/// other than `own` (what the path names), in this crate and those it names as dependencies,
/// that `module` can import (a foreign `#[doc(hidden)]` one not) and the crate's own it can't.
pub(crate) fn imports(
    cx: &Cx<'_, '_>,
    module: hir::Module,
    source: PathSource,
    name: &str,
    own: Option<ModuleDef>,
) -> Imports {
    let ns = source.ns();
    let cfg = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    // alloc: the error's parts, for an error hir found
    let mut found = Imports { ns, accessible: Vec::new(), inaccessible: Vec::new() };
    for decl in cx.paths().decls(cx).named(name) {
        let def = decl.def;
        if own == Some(def) {
            continue;
        }
        let Some(descr) = def_descr(cx, ns, def).filter(|it| source.is_expected(it)) else { continue };
        let item = match ns {
            Ns::Value => hir::ItemInNs::Values(def),
            Ns::Type => hir::ItemInNs::Types(def),
        };
        match module.find_use_path(cx.db, item, hir::PrefixKind::ByCrate, cfg) {
            Some(path) if decl.local || !is_doc_hidden(cx, def) => {
                // alloc: the error's text, for an error hir found
                let path = path.display(cx.db, cx.edition).to_string();
                let of_crate = path.starts_with("crate::");
                found.accessible.push(Import { path: in_edition(cx, path), of_crate, descr, def });
            }
            Some(_) => {}
            None if decl.local => {
                // alloc: the error's text, for an error hir found
                let path = def.canonical_path(cx.db, cx.edition).unwrap_or_else(|| name.to_owned());
                // alloc: the error's text, for an error hir found
                let path = in_edition(cx, format!("crate::{path}"));
                found.inaccessible.push(Import { path, of_crate: true, descr, def });
            }
            None => {}
        }
    }
    found
}

/// rustc's `show_candidates` of what [`imports`] found: the `use` to add, or the items that
/// exist but are not accessible; `instead` when the path named something else.
pub(crate) fn show_imports(cx: &Cx<'_, '_>, diag: &mut Diag, module: hir::Module, found: Imports, instead: bool) {
    let Imports { ns, mut accessible, mut inaccessible } = found;
    let instead = if instead { " instead" } else { "" };
    for found in [&mut accessible, &mut inaccessible] {
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found.dedup_by(|a, b| a.path == b.path);
        let (mut local, rest): (Vec<Import>, Vec<Import>) = found.drain(..).partition(|it| it.of_crate);
        let (core, rest): (Vec<Import>, Vec<Import>) = rest.into_iter().partition(|it| it.path.starts_with("core::"));
        let (std, foreign): (Vec<Import>, Vec<Import>) = rest.into_iter().partition(|it| it.path.starts_with("std::"));
        let both = std.len() == core.len();
        local.extend(std);
        if !both {
            local.extend(core);
        }
        local.extend(foreign);
        *found = local;
    }
    if !accessible.is_empty() {
        let msg = match accessible.as_slice() {
            // alloc: the error's text, for an error hir found
            [only] => format!("consider importing this {}{instead}", only.descr),
            many => {
                let kind = match many.iter().all(|it| it.descr == many[0].descr) {
                    true => many[0].descr,
                    false => "item",
                };
                let s = if kind.ends_with('s') { "es" } else { "s" };
                // alloc: the error's text, for an error hir found
                format!("consider importing one of these {kind}{s}{instead}")
            }
        };
        match use_placement(cx, module) {
            Some((span, found_use)) => {
                let newline = if found_use { "" } else { "\n" };
                diag.suggestions(
                    span,
                    msg,
                    // alloc: the error's text, for an error hir found
                    accessible.iter().map(|it| format!("use {};\n{newline}", it.path)),
                    Applicability::MaybeIncorrect,
                    SuggestionStyle::ShowAlways,
                );
            }
            None => {
                // alloc: the error's text, for an error hir found
                let paths: String = accessible.iter().map(|it| format!("\n{}", it.path)).collect();
                // alloc: the error's text, for an error hir found
                diag.help(format!("{msg}:{paths}"));
            }
        }
    }
    let span_of = |it: &Import| def_span(cx, ns, it.def);
    match inaccessible.as_slice() {
        [] => {}
        [only] => {
            // alloc: the error's text, for an error hir found
            let msg = format!("{} `{}` exists but is inaccessible", only.descr, only.path);
            match span_of(only) {
                Some(span) => {
                    // clone: on the error path, for an error hir found
                    let mut multi = MultiSpan::of(span.clone());
                    // alloc: the error's text, for an error hir found
                    multi.labels.push((span, "not accessible".to_owned()));
                    diag.span_note(multi, msg);
                }
                None => {
                    diag.note(msg);
                }
            }
        }
        many => {
            let descr = match many.iter().all(|it| it.descr == many[0].descr) {
                true => many[0].descr,
                false => "item",
            };
            // alloc: the error's text, for an error hir found
            let plural = if descr.ends_with('s') { format!("{descr}es") } else { format!("{descr}s") };
            // alloc: the error's text, for an error hir found
            let mut msg = format!("these {plural} exist but are inaccessible");
            let mut multi = MultiSpan::default();
            let mut colon = false;
            for it in many {
                match span_of(it) {
                    Some(span) => {
                        // clone: on the error path, for an error hir found
                        multi.primary.push(span.clone());
                        // alloc: the error's text, for an error hir found
                        multi.labels.push((span, format!("`{}`: not accessible", it.path)));
                    }
                    None => {
                        if !colon {
                            msg.push(':');
                            colon = true;
                        }
                        msg.push('\n');
                        msg.push_str(&it.path);
                    }
                }
            }
            diag.span_note(multi, msg);
        }
    }
}

/// Whether an item is `#[doc(hidden)]`, which rustc does not suggest importing from another
/// crate.
fn is_doc_hidden(cx: &Cx<'_, '_>, def: ModuleDef) -> bool {
    match def {
        ModuleDef::Function(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::Const(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::Static(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::Adt(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::EnumVariant(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::Trait(it) => it.attrs(cx.db).is_doc_hidden(),
        ModuleDef::TypeAlias(it) => it.attrs(cx.db).is_doc_hidden(),
        _ => false,
    }
}

/// A path to import as the crate's edition writes it: from the crate root without `crate::`
/// in 2015.
fn in_edition(cx: &Cx<'_, '_>, path: String) -> String {
    match (cx.edition, path.strip_prefix("crate::")) {
        // alloc: the error's text, for an error hir found
        (Edition::Edition2015, Some(rest)) => rest.to_owned(),
        _ => path,
    }
}

/// rustc's `UsePlacementFinder`: where a `use` goes in the module (not a block) `module` is
/// in, the first `use` item there or else its first item, and whether it was a `use`.
fn use_placement(cx: &Cx<'_, '_>, module: hir::Module) -> Option<(Span, bool)> {
    let module = module.nearest_non_block_module(cx.db);
    let source = module.definition_source_view(cx.db);
    fn first<'t>(mut items: impl Iterator<Item = ast::Item<'t>> + Clone) -> Option<(ast::Item<'t>, bool)> {
        // clone: an iterator of views, copies two indices
        match items.clone().find(|it| matches!(it, ast::Item::Use(_))) {
            Some(it) => Some((it, true)),
            None => items.next().map(|it| (it, false)),
        }
    }
    let (item, found_use) = match source.value {
        hir::ModuleSourceView::SourceFile(it) => first(it.items())?,
        hir::ModuleSourceView::Module(it) => first(it.item_list()?.items())?,
        hir::ModuleSourceView::BlockExpr(_) => return None,
    };
    let start = item
        .syntax()
        .children_with_tokens()
        .find(|it| match *it {
            syntax::NodeOrToken::Node(_) => true,
            syntax::NodeOrToken::Token(token) => {
                !token.kind().is_trivia() || ast::AnyComment::cast(token).is_some_and(|it| it.kind().doc.is_some())
            }
        })
        .map_or(item.syntax().text_range().start(), |it| it.text_range().start());
    Some((cx.span(InFile::new(source.file_id, TextRange::empty(start))), found_use))
}
