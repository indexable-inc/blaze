//! E0599 of a method call: `x.f()` where no method `f` is found for the type of `x`. With
//! rustc's notes: an associated function of that name to call with `T::f()` instead, a field
//! of that name to call as `(x.f)()`, the other types the method is on, a method of a similar
//! name, the traits that would give it (to import, or to implement), and where the type is
//! defined. The parts shared with `T::f` (`unresolved_assoc_item`) are here.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/suggest.rs`
//! (`report_no_match_method_error`, `create_no_assoc_err`, `set_label_for_method_error`,
//! `suggest_static_method_candidates`, `note_candidates_on_method_error`,
//! `suggest_associated_call_syntax`, `suggest_calling_field_as_fn`,
//! `set_not_found_span_label`, `suggest_traits_to_import`, `suggest_valid_traits`,
//! `suggest_use_candidates`, `type_derefs_to_local`, `find_likely_intended_associated_item`,
//! `suggest_method_not_found_because_of_unsatisfied_bounds`),
//! `compiler/rustc_hir_typeck/src/method/probe.rs` (`probe_for_similar_candidate`,
//! `impl_or_trait_item`, `candidate_method_names`), `compiler/rustc_hir_typeck/src/diagnostics.rs`
//! (`NoAssociatedItem`, `CandidateTraitNote`) and `compiler/rustc_parse/src/parser/item.rs`
//! (`inject_use_span`) (MIT or Apache-2.0).

use blaze_hash::HashSet;
use blaze_items::{Fact, ItemId};
use hir::{AsAssocItem as _, HasVisibility as _, InFile};
use stdx::edit_distance::{
    edit_distance_with_substrings, find_best_match_for_name_with_substrings,
};
use syntax::{
    SyntaxKind, T, TextRange,
    view::{self as ast, AstNode as _, HasArgList as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lints::code_fact,
    lower::{
        Found, Phase,
        unresolved_field::{fn_def_span, type_param_span},
    },
    model::{Applicability, Diag, Span, Suggestion, SuggestionStyle},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::UnresolvedMethodCall<'db>) -> Vec<Found> {
    let node = cx.node_view(d.expr);
    // alloc: the error's parts, for an error hir found
    let Some(ast::Expr::MethodCallExpr(call)) = node.value.left() else { return vec![] };
    // alloc: empty, allocates nothing
    let Some(name_ref) = call.name_ref() else { return vec![] };
    let at = |range: TextRange| cx.span(InFile::new(node.file_id, range));
    // alloc: the error's parts, for an error hir found
    let args: Vec<ast::Expr<'_>> = call.arg_list().map(|it| it.args().collect()).unwrap_or_default();
    let no_match = NoMatch {
        // clone: on the error path, for an error hir found
        receiver: d.receiver.clone(),
        // clone: on the error path, for an error hir found
        name: d.name.clone(),
        name_span: at(name_ref.syntax().text_range()),
        sugg_span: at(call.syntax().text_range()),
        scope: cx.vsema.scope(call.syntax()),
        is_method: true,
        args: Some(args.len()),
    };
    let name = no_match.name();
    let item_kind = no_match.item_kind();
    let mut diag = no_match.error(cx, item_kind);
    if call.syntax().text().contains('\n') {
        let lo = no_match.name_span.lo;
        diag.span_label(no_match.sugg_span.with_hi(lo), "");
    }

    let statics = static_candidates(cx, &no_match, d.assoc_func_with_same_name);
    if !statics.is_empty() {
        diag.note(
            "found the following associated functions; to be used as methods, functions must have a `self` parameter",
        );
        // clone: on the error path, for an error hir found
        diag.span_label(no_match.name_span.clone(), "this is an associated function, not a method");
    }
    if let [only] = statics.as_slice() {
        associated_call_syntax(cx, &mut diag, &no_match, only, call, &args, &at);
    }
    note_candidates(cx, &mut diag, &statics);

    let similar = similar_candidate(cx, &no_match);
    let field_as_fn = call_field_as_fn(cx, &mut diag, &no_match, call, &at);
    if !field_as_fn && similar.is_none() && statics.is_empty() {
        not_found_label(cx, &mut diag, &no_match, item_kind);
    }
    traits_to_import(cx, &mut diag, &no_match, &statics);
    if let Some(similar) = similar
        && no_match.name_span.expansion.is_none()
    {
        likely_intended(cx, &mut diag, &no_match, similar);
    }
    not_found_for_this(cx, &mut diag, &no_match, item_kind);

    let ty = cx.ty(&d.receiver);
    // alloc: the error's text, for an error hir found
    diag.fact_opt(cx.method_fact(&d.receiver, &name), format!("what `{name}` is on `{ty}`"));
    if let Some(field) = &d.field_with_same_name {
        // alloc: the error's text, for an error hir found
        diag.fact_opt(cx.method_fact(field, "call"), format!("whether the field `{name}` can be called"));
    }
    code_fact(cx, &mut diag, d.expr.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// What rustc's `report_no_match_method_error` is given: the type an item was looked for on,
/// its name, and where it was looked for.
pub(crate) struct NoMatch<'db> {
    pub(crate) receiver: hir::Type<'db>,
    pub(crate) name: hir::Name,
    pub(crate) name_span: Span,
    /// The whole call or path.
    pub(crate) sugg_span: Span,
    pub(crate) scope: Option<hir::SemanticsScope<'db>>,
    pub(crate) is_method: bool,
    /// The arguments of the call the item is called by, if it is.
    pub(crate) args: Option<usize>,
}

/// A function without `self` of the name looked for: rustc's static candidates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Static {
    Impl(hir::Impl, hir::Function),
    Trait(hir::Trait, hir::Function),
}

impl<'db> NoMatch<'db> {
    pub(crate) fn name(&self) -> String {
        // alloc: the error's text, for an error hir found
        self.name.as_str().to_owned()
    }

    /// rustc's `item_kind`.
    pub(crate) fn item_kind(&self) -> &'static str {
        if self.is_method {
            "method"
        } else if matches!(self.receiver.as_adt(), Some(hir::Adt::Enum(_))) {
            "variant, associated function, or constant"
        } else {
            "associated function or constant"
        }
    }

    /// rustc's `create_no_assoc_err`: the type as declared when it is a generic ADT and no
    /// impl of it has the item.
    pub(crate) fn error(&self, cx: &Cx<'_, 'db>, item_kind: &str) -> Diag {
        let mut ty = cx.ty(&self.receiver);
        if let Some((adt, args)) = self.receiver.as_adt_with_args()
            && !args.is_empty()
        {
            // alloc: the error's parts, for an error hir found
            let steps: Vec<hir::Type<'db>> = self.receiver.autoderef(cx.db).collect();
            let found = steps.iter().any(|ty| inherent_value(cx, ty, &self.name).is_some());
            if !found && steps.len() <= 1 {
                ty = cx.ty(&adt.ty(cx.db));
            }
        }
        let prefix = crate::print::prefix_string(cx, &self.receiver);
        let name = self.name.display(cx.db, cx.edition);
        // alloc: the error's text, for an error hir found
        let message = format!("no {item_kind} named `{name}` found for {prefix} `{ty}` in the current scope");
        // clone: on the error path, for an error hir found
        Diag::error(Some("E0599"), message, self.name_span.clone())
    }

    fn module(&self) -> Option<hir::Module> {
        self.scope.as_ref().map(|it| it.module())
    }
}

/// An associated function or constant named `name` of an inherent impl of `ty`'s type.
fn inherent_value<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>, name: &hir::Name) -> Option<hir::AssocItem> {
    ty.as_adt()?;
    ty.iterate_assoc_items(cx.db, |item| {
        let inherent = matches!(item.container(cx.db), hir::AssocItemContainer::Impl(it) if it.trait_(cx.db).is_none());
        let value = matches!(item, hir::AssocItem::Function(_) | hir::AssocItem::Const(_));
        (inherent && value && item.name(cx.db).as_ref() == Some(name)).then_some(item)
    })
}

/// Every trait of every crate: rustc's `all_traits`.
pub(crate) fn all_traits(cx: &Cx<'_, '_>) -> Vec<hir::Trait> {
    // alloc: the error's parts, for an error hir found
    let mut traits = Vec::new();
    for krate in hir::Crate::all(cx.db) {
        for module in krate.modules_iter(cx.db) {
            for def in module.declarations_iter(cx.db) {
                if let hir::ModuleDef::Trait(it) = def {
                    traits.push(it);
                }
            }
        }
    }
    traits
}

fn is_local_fn(cx: &Cx<'_, '_>, f: hir::Function) -> bool {
    f.module(cx.db).krate(cx.db) == cx.krate
}

/// The functions without `self` named as the method in the inherent impls of the types the
/// receiver derefs to and in the traits in scope.
fn static_candidates(cx: &Cx<'_, '_>, no_match: &NoMatch<'_>, found: Option<hir::Function>) -> Vec<Static> {
    // alloc: the error's parts, for an error hir found
    let mut statics = Vec::new();
    let mut push = |it: Static| {
        if !statics.contains(&it) {
            statics.push(it);
        }
    };
    for ty in no_match.receiver.autoderef(cx.db) {
        ty.iterate_assoc_items(cx.db, |item| {
            if let hir::AssocItem::Function(f) = item
                && f.name(cx.db) == no_match.name
                && !f.has_self_param(cx.db)
                && let hir::AssocItemContainer::Impl(it) = item.container(cx.db)
                && it.trait_(cx.db).is_none()
            {
                push(Static::Impl(it, f));
            }
            None::<()>
        });
    }
    if let Some(scope) = &no_match.scope {
        for t in all_traits(cx).into_iter().filter(|it| scope.can_use_trait_methods(*it)) {
            for item in t.items_iter(cx.db) {
                if let hir::AssocItem::Function(f) = item
                    && f.name(cx.db) == no_match.name
                    && !f.has_self_param(cx.db)
                {
                    push(Static::Trait(t, f));
                }
            }
        }
    }
    if let Some(f) = found
        && !f.has_self_param(cx.db)
        && let Some(item) = f.as_assoc_item(cx.db)
    {
        match item.container(cx.db) {
            hir::AssocItemContainer::Impl(it) if it.trait_(cx.db).is_none() => push(Static::Impl(it, f)),
            hir::AssocItemContainer::Trait(it) => push(Static::Trait(it, f)),
            hir::AssocItemContainer::Impl(_) => {}
        }
    }
    statics
}

/// rustc's `suggest_associated_call_syntax`: `T::f(args)` for `x.f(args)`, with `x` first when
/// `f` takes the type as its first argument.
fn associated_call_syntax<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    no_match: &NoMatch<'db>,
    candidate: &Static,
    call: ast::MethodCallExpr<'_>,
    args: &[ast::Expr<'_>],
    at: &impl Fn(TextRange) -> Span,
) {
    let (ty_str, first_arg) = match *candidate {
        Static::Impl(imp, f) => {
            let impl_ty = imp.self_ty(cx.db);
            let target = no_match
                .receiver
                .autoderef(cx.db)
                .find(|ty| match (ty.as_adt(), impl_ty.as_adt()) {
                    (Some(a), Some(b)) => a == b,
                    _ => ty.could_unify_with(cx.db, &impl_ty),
                })
                // clone: on the error path, for an error hir found
                .unwrap_or_else(|| impl_ty.clone())
                .strip_references();
            let impl_ty_str = cx.ty(&impl_ty);
            let first_arg = f.assoc_fn_params(cx.db).first().and_then(|first| {
                let ty = first.ty();
                (cx.ty(&ty.strip_references()) == impl_ty_str).then(|| match ty.as_reference() {
                    Some((_, hir::Mutability::Mut)) => "&mut ",
                    Some((_, hir::Mutability::Shared)) => "&",
                    None => "",
                })
            });
            (value_string(cx, &target), first_arg)
        }
        Static::Trait(..) => (value_string(cx, &no_match.receiver.strip_references()), None),
    };
    let mut applicability = Applicability::MachineApplicable;
    let explicit = first_arg.and(call.receiver()).into_iter().chain(args.iter().copied());
    let snippets: Vec<String> = explicit
        .map(|arg| match cx.snippet(&at(arg.syntax().text_range())) {
            // alloc: the error's text, for an error hir found
            Some(text) => text.to_owned(),
            None => {
                applicability = Applicability::HasPlaceholders;
                // alloc: the error's text, for an error hir found
                "_".to_owned()
            }
        })
        // alloc: the error's parts, for an error hir found
        .collect();
    // alloc: the error's text, for an error hir found
    let args = format!("({}{})", first_arg.unwrap_or(""), snippets.join(", "));
    let name = no_match.name.display(cx.db, cx.edition);
    diag.suggestion_verbose(
        // clone: on the error path, for an error hir found
        no_match.sugg_span.clone(),
        "use associated function syntax instead",
        // alloc: the error's text, for an error hir found
        format!("{ty_str}::{name}{args}"),
        applicability,
    );
}

/// rustc's `ty_to_value_string`: a type as a path in an expression, `Foo::<u8>`.
pub(crate) fn value_string<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> String {
    let printed = cx.ty(ty);
    match (ty.as_adt(), printed.find('<')) {
        // alloc: the error's text, for an error hir found
        (Some(_), Some(at)) => format!("{}::{}", &printed[..at], &printed[at..]),
        _ => printed,
    }
}

/// rustc's `note_candidates_on_method_error`: where each candidate is defined.
fn note_candidates(cx: &Cx<'_, '_>, diag: &mut Diag, sources: &[Static]) {
    let mut sources: Vec<(u8, String, Static)> = sources
        .iter()
        .map(|it| match *it {
            Static::Trait(t, _) => (0, cx.def_path(hir::ModuleDef::Trait(t)), *it),
            Static::Impl(imp, _) => (1, cx.ty(&imp.self_ty(cx.db)), *it),
        })
        // alloc: the error's parts, for an error hir found
        .collect();
    sources.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    sources.dedup_by(|a, b| a.2 == b.2);
    let limit = if sources.len() == 5 { 5 } else { 4 };
    let many = sources.len() > 1;
    for (idx, (_, _, source)) in sources.iter().take(limit).enumerate() {
        match *source {
            Static::Impl(imp, f) => {
                let note_span = match is_local_fn(cx, f) {
                    true => fn_def_span(cx, f),
                    false if imp.module(cx.db).krate(cx.db) == cx.krate => cx
                        .vsema
                        .source(imp)
                        .map(|it| cx.span(InFile::new(it.file_id, def_range(it.value.syntax())))),
                    false => None,
                };
                let impl_ty = cx.ty(&imp.self_ty(cx.db));
                let insertion = match imp.trait_(cx.db) {
                    // alloc: the error's text, for an error hir found
                    Some(t) => format!(" of the trait `{}`", cx.def_path(hir::ModuleDef::Trait(t))),
                    // alloc: the error's text, for an error hir found
                    None => String::new(),
                };
                let note = match many {
                    // alloc: the error's text, for an error hir found
                    true => format!("candidate #{} is defined in an impl{insertion} for the type `{impl_ty}`", idx + 1),
                    // alloc: the error's text, for an error hir found
                    false => format!("the candidate is defined in an impl{insertion} for the type `{impl_ty}`"),
                };
                match note_span {
                    Some(span) => diag.span_note(span, note),
                    None => diag.note(note),
                };
            }
            Static::Trait(t, f) => {
                let path = cx.def_path(hir::ModuleDef::Trait(t));
                let note = match many {
                    // alloc: the error's text, for an error hir found
                    true => format!("candidate #{} is defined in the trait `{path}`", idx + 1),
                    // alloc: the error's text, for an error hir found
                    false => format!("the candidate is defined in the trait `{path}`"),
                };
                match fn_def_span(cx, f) {
                    Some(span) => diag.span_note(span, note),
                    None => diag.note(note),
                };
            }
        }
    }
    if sources.len() > limit {
        // alloc: the error's text, for an error hir found
        diag.note(format!("and {} others", sources.len() - limit));
    }
}

/// rustc's `suggest_calling_field_as_fn`: `(x.f)()` when `f` is a field of a callable type.
fn call_field_as_fn(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    no_match: &NoMatch<'_>,
    call: ast::MethodCallExpr<'_>,
    at: &impl Fn(TextRange) -> Span,
) -> bool {
    let found = no_match.receiver.autoderef(cx.db).find_map(|ty| {
        if !matches!(ty.as_adt()?, hir::Adt::Struct(_) | hir::Adt::Union(_)) {
            return None;
        }
        ty.fields(cx.db).into_iter().find(|(field, _)| field.name(cx.db) == no_match.name)
    });
    let Some((field, field_ty)) = found else { return false };
    let accessible = no_match.module().is_none_or(|module| field.is_visible_from(cx.db, module));
    if accessible {
        let what = field_ty.autoderef(cx.db).find_map(|ty| callable_descr(cx, &ty));
        let name = no_match.name.display(cx.db, cx.edition);
        let name_range = call.name_ref().map(|it| it.syntax().text_range());
        match (what, call.receiver(), name_range) {
            (Some(what), Some(receiver), Some(name_range)) => {
                let expr = at(TextRange::new(receiver.syntax().text_range().start(), name_range.end()));
                diag.multipart_suggestion(
                    // alloc: the error's text, for an error hir found
                    format!("to call the {what} stored in `{name}`, surround the field access with parentheses"),
                    // alloc: the error's text, for an error hir found
                    vec![(expr.shrink_to_lo(), "(".to_owned()), (expr.shrink_to_hi(), ")".to_owned())],
                    Applicability::MachineApplicable,
                );
            }
            (None, _, Some(name_range)) => {
                let end = call.syntax().text_range().end();
                if name_range.end() < end {
                    diag.suggestion(
                        at(TextRange::new(name_range.end(), end)),
                        "remove the arguments",
                        "",
                        Applicability::MaybeIncorrect,
                    );
                }
            }
            _ => {}
        }
    }
    let kind = if accessible { "field" } else { "private field" };
    // alloc: the error's text, for an error hir found
    diag.span_label(no_match.name_span.clone(), format!("{kind}, not a method"));
    true
}

/// What a value of `ty` is when it can be called: rustc's `extract_callable_info` and
/// `def_descr`.
fn callable_descr(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<&'static str> {
    if ty.as_dyn_trait().is_some() {
        return ty.as_callable(cx.db).map(|_| "trait object");
    }
    let callable = ty.as_callable(cx.db)?;
    Some(match callable.kind() {
        hir::CallableKind::Function(f) => match (f.as_assoc_item(cx.db), f.has_self_param(cx.db)) {
            (Some(_), true) => "method",
            (Some(_), false) => "associated function",
            (None, _) => "function",
        },
        hir::CallableKind::TupleStruct(_) => "tuple struct",
        hir::CallableKind::TupleEnumVariant(_) => "tuple variant",
        hir::CallableKind::Closure(_) => "closure",
        hir::CallableKind::FnPtr => "function pointer",
        hir::CallableKind::FnImpl(_) if ty.as_type_param(cx.db).is_some() => "type parameter",
        hir::CallableKind::FnImpl(_) => "opaque type",
    })
}

/// rustc's `set_not_found_span_label`: "method not found in `T`", and the types the method
/// is on instead.
pub(crate) fn not_found_label<'db>(cx: &Cx<'_, 'db>, diag: &mut Diag, no_match: &NoMatch<'db>, item_kind: &str) {
    let ty = cx.ty(&no_match.receiver);
    // alloc: the error's text, for an error hir found
    diag.span_label(no_match.name_span.clone(), format!("{item_kind} not found in `{ty}`"));
    let string_or_str = match no_match.receiver.as_reference() {
        Some((inner, _)) => inner.is_str() || is_string(cx, &inner),
        None => is_string(cx, &no_match.receiver),
    };
    if string_or_str && no_match.name.as_str() == "iter" {
        diag.suggestion_verbose(
            // clone: on the error path, for an error hir found
            no_match.name_span.clone(),
            "because of the in-memory representation of `&str`, to obtain an `Iterator` over each of its codepoint use method `chars`",
            "chars",
            Applicability::MachineApplicable,
        );
    }
    if no_match.receiver.as_adt().is_none() {
        return;
    }
    let receiver_str = cx.ty(&no_match.receiver);
    // alloc: the error's parts, for an error hir found
    let mut found: Vec<String> = Vec::new();
    no_match.receiver.iterate_assoc_items(cx.db, |item| {
        if item.name(cx.db).as_ref() != Some(&no_match.name) {
            return None::<()>;
        }
        let hir::AssocItemContainer::Impl(imp) = item.container(cx.db) else { return None };
        if imp.trait_(cx.db).is_some() {
            return None;
        }
        let is_method = matches!(item, hir::AssocItem::Function(f) if f.has_self_param(cx.db));
        let value = matches!(item, hir::AssocItem::Function(_) | hir::AssocItem::Const(_));
        let impl_ty = cx.ty(&imp.self_ty(cx.db));
        let take = match (no_match.is_method, is_method) {
            (true, true) => impl_ty != receiver_str,
            (false, false) => value,
            _ => false,
        };
        if take {
            found.push(impl_ty);
        }
        None
    });
    found.sort();
    found.dedup();
    match found.as_slice() {
        [] => {}
        [only] => {
            // alloc: the error's text, for an error hir found
            diag.note(format!("the {item_kind} was found for `{only}`"));
        }
        candidates => {
            let limit = if candidates.len() == 5 { 5 } else { 4 };
            // alloc: the error's text, for an error hir found
            let types: Vec<String> = candidates.iter().take(limit).map(|it| format!("- `{it}`")).collect();
            let more = match candidates.len() > limit {
                // alloc: the error's text, for an error hir found
                true => format!("\nand {} more types", candidates.len() - limit),
                // alloc: the error's text, for an error hir found
                false => String::new(),
            };
            // alloc: the error's text, for an error hir found
            diag.note(format!("the {item_kind} was found for\n{}{more}", types.join("\n")));
        }
    }
}

fn is_string(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    let items = hir_def::lang_item::lang_items(cx.db, cx.krate.base());
    match (ty.as_adt(), items.String) {
        (Some(hir::Adt::Struct(it)), Some(string)) => it == hir::Struct::from(string),
        _ => false,
    }
}

/// rustc's `probe_for_similar_candidate`: the method (for a call) or associated function or
/// constant (for a path) of the type, from its impls and every trait, whose name is closest.
pub(crate) fn similar_candidate<'db>(cx: &Cx<'_, 'db>, no_match: &NoMatch<'db>) -> Option<hir::AssocItem> {
    let scope = no_match.scope.as_ref()?;
    let lookup = no_match.name.as_str();
    let max_dist = std::cmp::max(lookup.len(), 3) / 3;
    // alloc: the error's parts, for an error hir found
    let traits: HashSet<hir_def::TraitId> = all_traits(cx).into_iter().map(Into::into).collect();
    // alloc: the error's parts, for an error hir found
    let mut found: Vec<hir::AssocItem> = Vec::new();
    let mut close = |item: hir::AssocItem| {
        if let Some(name) = item.name(cx.db)
            && edit_distance_with_substrings(lookup, name.as_str(), max_dist).is_some_and(|it| it > 0)
            && !found.iter().any(|it| it.name(cx.db).as_ref() == Some(&name))
        {
            found.push(item);
        }
    };
    if no_match.is_method {
        no_match.receiver.iterate_method_candidates_with_traits(cx.db, scope, &traits, None, |f| {
            if let Some(item) = f.as_assoc_item(cx.db) {
                close(item);
            }
            None::<()>
        });
    } else {
        no_match.receiver.iterate_path_candidates(cx.db, scope, &traits, None, |item| {
            if matches!(item, hir::AssocItem::Function(_) | hir::AssocItem::Const(_)) {
                close(item);
            }
            None::<()>
        });
    }
    found.sort_by(|a, b| a.name(cx.db).cmp(&b.name(cx.db)));
    // alloc: the error's text, for an error hir found
    let names: Vec<String> = found.iter().filter_map(|it| Some(it.name(cx.db)?.as_str().to_owned())).collect();
    let best = find_best_match_for_name_with_substrings(&names, lookup, None).cloned()?;
    found.into_iter().find(|it| it.name(cx.db).is_some_and(|name| name.as_str() == best))
}

/// rustc's `find_likely_intended_associated_item`: "there is a method `g` with a similar
/// name".
pub(crate) fn likely_intended(cx: &Cx<'_, '_>, diag: &mut Diag, no_match: &NoMatch<'_>, similar: hir::AssocItem) {
    let Some(name) = similar.name(cx.db) else { return };
    // alloc: the error's text, for an error hir found
    let name = name.display(cx.db, cx.edition).to_string();
    let (an, descr) = match similar {
        hir::AssocItem::Function(f) if f.has_self_param(cx.db) => ("a", "method"),
        hir::AssocItem::Function(_) => ("an", "associated function"),
        hir::AssocItem::Const(_) => ("an", "associated constant"),
        hir::AssocItem::TypeAlias(_) => ("an", "associated type"),
    };
    // alloc: the error's text, for an error hir found
    let message = format!("there is {an} {descr} `{name}` with a similar name");
    let def_span = |cx: &Cx<'_, '_>| match similar {
        hir::AssocItem::Function(f) => fn_def_span(cx, f),
        hir::AssocItem::Const(c) => {
            cx.vsema.source(c).map(|it| cx.span(InFile::new(it.file_id, def_range(it.value.syntax()))))
        }
        hir::AssocItem::TypeAlias(t) => {
            cx.vsema.source(t).map(|it| cx.span(InFile::new(it.file_id, def_range(it.value.syntax()))))
        }
    };
    let help = |diag: &mut Diag, message: String| match def_span(cx) {
        Some(span) => {
            diag.span_help(span, message);
        }
        None => {
            diag.help(message);
        }
    };
    match similar {
        hir::AssocItem::Function(f) => {
            let params = f.params_without_self(cx.db).len();
            let is_method = f.has_self_param(cx.db);
            let inputs = if is_method { params } else { f.assoc_fn_params(cx.db).len() };
            match no_match.args {
                Some(args) if inputs == args => {
                    // clone: on the error path, for an error hir found
                    diag.suggestion_verbose(no_match.name_span.clone(), message, name, Applicability::MaybeIncorrect);
                }
                // alloc: the error's text, for an error hir found
                Some(_) if is_method => help(diag, format!("{message}, but with different arguments")),
                _ => help(diag, message),
            }
        }
        _ if !no_match.is_method && no_match.args.is_none_or(|it| it == 0) => {
            // clone: on the error path, for an error hir found
            diag.suggestion_verbose(no_match.name_span.clone(), message, name, Applicability::MaybeIncorrect);
        }
        _ => help(diag, message),
    }
}

/// rustc's `suggest_method_not_found_because_of_unsatisfied_bounds` without unsatisfied
/// bounds: "method `f` not found for this struct" at a local type's definition, or at a type
/// parameter.
pub(crate) fn not_found_for_this(cx: &Cx<'_, '_>, diag: &mut Diag, no_match: &NoMatch<'_>, item_kind: &str) {
    let span = if let Some(param) = no_match.receiver.as_type_param(cx.db) {
        type_param_span(cx, param)
    } else if let Some(adt) = no_match.receiver.as_adt()
        && adt.module(cx.db).krate(cx.db) == cx.krate
    {
        cx.vsema.source(adt).map(|it| cx.span(InFile::new(it.file_id, def_range(it.value.syntax()))))
    } else {
        None
    };
    if let Some(span) = span {
        let prefix = crate::print::prefix_string(cx, &no_match.receiver);
        let name = no_match.name.display(cx.db, cx.edition);
        // alloc: the error's text, for an error hir found
        diag.span_label(span, format!("{item_kind} `{name}` not found for this {prefix}"));
    }
}

/// rustc's `suggest_traits_to_import`: the traits not in scope that give the item (to
/// import), else the traits that have an item of the name (to implement).
pub(crate) fn traits_to_import<'db>(cx: &Cx<'_, 'db>, diag: &mut Diag, no_match: &NoMatch<'db>, statics: &[Static]) {
    let Some(scope) = &no_match.scope else { return };
    let all = all_traits(cx);
    let out_of_scope: Vec<hir::Trait> = all
        .iter()
        .copied()
        .filter(|t| !scope.can_use_trait_methods(*t) && trait_applies(cx, no_match, scope, *t))
        // alloc: the error's parts, for an error hir found
        .collect();
    if valid_traits(cx, diag, no_match, out_of_scope, true) {
        return;
    }
    let is_local_ty = |ty: &hir::Type<'_>| {
        if let Some(adt) = ty.as_adt() {
            return adt.module(cx.db).krate(cx.db) == cx.krate;
        }
        if let Some(t) = ty.as_dyn_trait() {
            return t.module(cx.db).krate(cx.db) == cx.krate;
        }
        ty.as_type_param(cx.db).is_some()
    };
    let type_is_local = match no_match.is_method {
        true => no_match.receiver.autoderef(cx.db).any(|ty| is_local_ty(&ty)),
        false => is_local_ty(&no_match.receiver),
    };
    let mut candidates: Vec<(bool, String, hir::Trait)> = all
        .into_iter()
        .filter(|t| {
            statics.iter().all(|it| match *it {
                Static::Trait(other, _) => other != *t,
                Static::Impl(imp, _) => imp.trait_(cx.db) != Some(*t),
            })
        })
        .filter(|t| {
            let local = t.module(cx.db).krate(cx.db) == cx.krate;
            (type_is_local || local)
                && !t.is_auto(cx.db)
                && t.items_iter(cx.db).any(|item| {
                    matches!(item, hir::AssocItem::Function(_) | hir::AssocItem::Const(_))
                        && item.name(cx.db).as_ref() == Some(&no_match.name)
                        && (local || matches!(t.visibility(cx.db), hir::Visibility::Public))
                })
        })
        .map(|t| (t.module(cx.db).krate(cx.db) != cx.krate, cx.def_path(hir::ModuleDef::Trait(t)), t))
        // alloc: the error's parts, for an error hir found
        .collect();
    if candidates.is_empty() {
        return;
    }
    candidates.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    candidates.dedup_by(|a, b| a.2 == b.2);
    let param = match no_match.receiver.as_reference() {
        Some((inner, _)) => inner.as_type_param(cx.db),
        None => no_match.receiver.as_type_param(cx.db),
    };
    diag.help(match param {
        Some(_) => "items from traits can only be used if the type parameter is bounded by the trait",
        None => "items from traits can only be used if the trait is implemented and in scope",
    });
    // alloc: the error's text, for an error hir found
    let name = no_match.name.display(cx.db, cx.edition).to_string();
    let len = candidates.len();
    let message = |action: &str| {
        // alloc: the error's text, for an error hir found
        format!(
            "the following {} an item `{name}`, perhaps you need to {action} {}:",
            if len == 1 { "trait defines" } else { "traits define" },
            if len == 1 { "it" } else { "one of them" },
        )
    };
    if let Some(param) = param {
        restrict_param(cx, diag, param, &candidates, &message);
        return;
    }
    let impls = |t: hir::Trait| no_match.receiver.impls_trait(cx.db, t, &[]);
    match candidates.as_slice() {
        [(false, path, t)] => {
            if impls(*t) {
                // alloc: the error's parts, for an error hir found
                valid_traits(cx, diag, no_match, vec![*t], false);
            } else {
                // alloc: the error's text, for an error hir found
                let note = format!("`{path}` defines an item `{name}`, perhaps you need to implement it");
                match cx.vsema.source(*t) {
                    Some(it) => diag.span_note(cx.span(InFile::new(it.file_id, def_range(it.value.syntax()))), note),
                    None => diag.note(note),
                };
            }
        }
        candidates => {
            let mut note = message("implement");
            for (i, (_, path, t)) in candidates.iter().enumerate() {
                if impls(*t) {
                    // alloc: the error's parts, for an error hir found
                    valid_traits(cx, diag, no_match, vec![*t], false);
                }
                // alloc: the error's text, for an error hir found
                note.push_str(&format!("\ncandidate #{}: `{path}`", i + 1));
            }
            diag.note(note);
        }
    }
}

/// The bound to add to the type parameter `param` for each trait.
fn restrict_param(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    param: hir::TypeParam,
    candidates: &[(bool, String, hir::Trait)],
    message: &impl Fn(&str) -> String,
) {
    if param.is_implicit(cx.db) {
        return;
    }
    let bounds = param.trait_bounds(cx.db);
    if candidates.iter().any(|(_, _, t)| bounds.contains(t)) {
        return;
    }
    let Some(source) = cx.vsema.source(param.merge()) else { return };
    let either::Either::Left(ast::TypeOrConstParam::Type(node)) = source.value else { return };
    let Some(name) = node.name() else { return };
    let at = |range: TextRange| cx.span(InFile::new(source.file_id, range));
    let last_bound = node.type_bound_list().and_then(|it| it.bounds().last());
    let (span, introducer) = match (last_bound, node.colon_token()) {
        (Some(bound), _) => (at(bound.syntax().text_range()).shrink_to_hi(), " +"),
        (None, Some(colon)) => (at(colon.text_range()).shrink_to_hi(), ""),
        (None, None) => (at(name.syntax().text_range()).shrink_to_hi(), ":"),
    };
    let mut applicability = Applicability::MaybeIncorrect;
    let substitutions = candidates
        .iter()
        .map(|(_, path, t)| {
            let params: Vec<String> = hir::GenericDef::Trait(*t)
                .type_or_const_params(cx.db)
                .into_iter()
                .filter(|it| match it.split(cx.db) {
                    either::Either::Right(it) => !it.is_implicit(cx.db) && it.default(cx.db).is_none(),
                    either::Either::Left(it) => it.default(cx.db, cx.display_target).is_none(),
                })
                // alloc: the error's text, for an error hir found
                .map(|it| it.name(cx.db).as_str().to_owned())
                // alloc: the error's parts, for an error hir found
                .collect();
            let candidate = match params.is_empty() {
                // clone: on the error path, for an error hir found
                true => path.clone(),
                false => {
                    applicability = Applicability::HasPlaceholders;
                    // alloc: the error's text, for an error hir found
                    format!("{path}</* {} */>", params.join(", "))
                }
            };
            // alloc: the error's text, for an error hir found
            vec![(span.clone(), format!("{introducer} {candidate}"))]
        })
        // alloc: the error's parts, for an error hir found
        .collect();
    let name = name.text();
    diag.suggestions.push(Suggestion {
        // alloc: the error's text, for an error hir found
        message: message(&format!("restrict type parameter `{name}` with")),
        substitutions,
        applicability,
        style: SuggestionStyle::ShowCode,
    });
}

/// Whether the item looked for is found on the receiver through `t` alone.
fn trait_applies<'db>(cx: &Cx<'_, 'db>, no_match: &NoMatch<'db>, scope: &hir::SemanticsScope<'db>, t: hir::Trait) -> bool {
    if !t.items_iter(cx.db).any(|it| it.name(cx.db).as_ref() == Some(&no_match.name)) {
        return false;
    }
    // alloc: the error's parts, for an error hir found
    let traits: HashSet<hir_def::TraitId> = std::iter::once(t.into()).collect();
    let of_trait = |item: hir::AssocItem| (item.container_or_implemented_trait(cx.db) == Some(t)).then_some(());
    match no_match.is_method {
        true => no_match
            .receiver
            .iterate_method_candidates_with_traits(cx.db, scope, &traits, Some(&no_match.name), |f| {
                of_trait(f.as_assoc_item(cx.db)?)
            })
            .is_some(),
        false => no_match
            .receiver
            .iterate_path_candidates(cx.db, scope, &traits, Some(&no_match.name), of_trait)
            .is_some(),
    }
}

/// rustc's `suggest_valid_traits`: `use` the traits that give the item.
pub(crate) fn valid_traits(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    no_match: &NoMatch<'_>,
    traits: Vec<hir::Trait>,
    explain: bool,
) -> bool {
    if traits.is_empty() {
        return false;
    }
    let Some(module) = no_match.module() else { return false };
    let mut candidates: Vec<(String, hir::Trait)> =
        // alloc: the error's parts, for an error hir found
        traits.into_iter().map(|t| (cx.def_path(hir::ModuleDef::Trait(t)), t)).collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup_by(|a, b| a.1 == b.1);
    // alloc: the error's text, for an error hir found
    let name = no_match.name.display(cx.db, cx.edition).to_string();
    let edition_fix = candidates.iter().map(|it| it.1).find(|t| is_try_into(cx, *t));
    if explain {
        diag.help("items from traits can only be used if the trait is in scope");
    }
    let this_trait_is = match candidates.as_slice() {
        // alloc: the error's text, for an error hir found
        [(_, t)] => format!("trait `{}` which provides `{name}` is", t.name(cx.db).display(cx.db, cx.edition)),
        // alloc: the error's text, for an error hir found
        _ => format!("the following traits which provide `{name}` are"),
    };
    // alloc: the error's text, for an error hir found
    let message = format!("{this_trait_is} implemented but not in scope");
    let (accessible, inaccessible): (Vec<_>, Vec<_>) =
        candidates.iter().map(|it| it.1).partition(|t| t.is_visible_from(cx.db, module));
    let sugg = |traits: &[hir::Trait], visible: bool| {
        let (prefix, postfix) = if visible { ("use ", ";") } else { ("", "") };
        let mut suggs: Vec<String> =
            // alloc: the error's text, for an error hir found
            traits.iter().map(|t| format!("{prefix}{}{postfix}\n", crate_path(cx, module, *t))).collect();
        suggs.sort();
        suggs
    };
    let accessible = sugg(&accessible, true);
    let inaccessible = sugg(&inaccessible, false);
    let Some(span) = inject_use_span(cx, module) else { return true };
    let for_access = |diag: &mut Diag, mut message: String, suggs: Vec<String>| {
        // alloc: the error's text, for an error hir found
        message += &format!("; perhaps you want to import {}", if suggs.len() == 1 { "it" } else { "one of them" });
        // clone: on the error path, for an error hir found
        diag.suggestions(span.clone(), message, suggs, Applicability::MaybeIncorrect, SuggestionStyle::ShowAlways);
    };
    let for_privacy = |diag: &mut Diag, suggs: Vec<String>| {
        let this_trait_is = match suggs.as_slice() {
            // alloc: the error's text, for an error hir found
            [only] => format!("trait `{}` which provides `{name}` is", only.trim()),
            // alloc: the error's text, for an error hir found
            _ => format!("the following traits which provide `{name}` are"),
        };
        // alloc: the error's text, for an error hir found
        let message = format!("{this_trait_is} implemented but not reachable");
        if suggs.len() == 1 {
            diag.help(message);
        } else {
            // clone: on the error path, for an error hir found
            diag.suggestions(span.clone(), message, suggs, Applicability::MaybeIncorrect, SuggestionStyle::ShowAlways);
        }
    };
    if accessible.is_empty() {
        for_privacy(diag, inaccessible);
    } else if inaccessible.is_empty() {
        for_access(diag, message, accessible);
    } else {
        for_access(diag, message, accessible);
        for_privacy(diag, inaccessible);
    }
    if let Some(t) = edition_fix {
        // alloc: the error's text, for an error hir found
        diag.note(format!("'{}' is included in the prelude starting in Edition 2021", crate_path(cx, module, t)));
    }
    true
}

fn is_try_into(cx: &Cx<'_, '_>, t: hir::Trait) -> bool {
    t.name(cx.db).as_str() == "TryInto"
        && t.module(cx.db).krate(cx.db).origin(cx.db).is_lang()
        && t.module(cx.db).name(cx.db).is_some_and(|it| it.as_str() == "convert")
}

/// rustc's `with_crate_prefix!(def_path_str(t))`: the path `t` is imported by from `module`,
/// starting at `crate::` when it is of this crate.
pub(crate) fn crate_path(cx: &Cx<'_, '_>, module: hir::Module, t: hir::Trait) -> String {
    let cfg = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let item = hir::ItemInNs::Types(hir::ModuleDef::Trait(t));
    if let Some(path) = module.find_use_path(cx.db, item, hir::PrefixKind::ByCrate, cfg) {
        // alloc: the error's text, for an error hir found
        return path.display(cx.db, cx.edition).to_string();
    }
    let path = hir::ModuleDef::Trait(t).canonical_path(cx.db, cx.edition).unwrap_or_default();
    let krate = t.module(cx.db).krate(cx.db);
    match krate == cx.krate {
        // alloc: the error's text, for an error hir found
        true => format!("crate::{path}"),
        false => match krate.display_name(cx.db) {
            // alloc: the error's text, for an error hir found
            Some(name) => format!("{name}::{path}"),
            None => path,
        },
    }
}

/// rustc's `ModSpans::inject_use_span` of the module `module` is in (not a block): where its
/// first token after its inner attributes starts.
pub(crate) fn inject_use_span(cx: &Cx<'_, '_>, module: hir::Module) -> Option<Span> {
    let module = module.nearest_non_block_module(cx.db);
    let source = module.definition_source_view(cx.db);
    let (body, end) = match source.value {
        hir::ModuleSourceView::SourceFile(it) => (it.syntax(), it.syntax().text_range().end()),
        hir::ModuleSourceView::Module(it) => {
            let list = it.item_list()?;
            let end = list.r_curly_token()?.text_range().start();
            (list.syntax(), end)
        }
        hir::ModuleSourceView::BlockExpr(_) => return None,
    };
    let mut tokens = body.descendants_with_tokens().filter_map(|it| it.into_token());
    if matches!(source.value, hir::ModuleSourceView::Module(_)) {
        tokens.next();
    }
    let start = tokens
        .find(|token| {
            let kind = token.kind();
            if kind.is_trivia() || kind == SyntaxKind::SHEBANG || kind == T![inner_doc_comment] {
                return false;
            }
            !token.parent_ancestors().any(|it| ast::Attr::cast(it).is_some_and(|it| it.excl_token().is_some()))
        })
        .map_or(end, |it| it.text_range().start());
    Some(cx.span(InFile::new(source.file_id, TextRange::empty(start))))
}

/// The fact of the item looked for on an ADT's own impls.
pub(crate) fn adt_fact(cx: &Cx<'_, '_>, diag: &mut Diag, adt: hir::Adt) {
    // alloc: the error's text, for an error hir found
    let name = adt.name(cx.db).display(cx.db, cx.edition).to_string();
    // alloc: the error's text, for an error hir found
    diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the items of `{name}`"));
}
