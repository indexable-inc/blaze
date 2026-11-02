//! E0282, E0283 and E0284: a type inference could not decide, or a goal on it that could be met
//! in more than one way. Like rustc, the error points at the best place to annotate: the `let`
//! whose binding has the undecided type, "type annotations needed" (or "type annotations needed
//! for `Vec<_>`" when the type has a known shape), with "consider giving `x` an explicit type,
//! where the type for type parameter `T` is specified" and its `: Vec<T>` or `: /* Type */`; or
//! the path or method whose generic parameter it is, "cannot infer type of the type parameter
//! `T` declared on the function `f`" with "consider specifying a concrete type for the type
//! parameter `T`" and its `::</* Type */>`; "type must be known at this point" where type
//! checking needed it; else "cannot infer type" (or "cannot infer type for type parameter `T`
//! declared on the function `f`") where it was found. An ambiguous goal adds rustc's notes:
//! "multiple `impl`s satisfying `T: Trait` found" on the impls and where clauses that could
//! meet it, "the type must implement `Trait`", "cannot satisfy `..`", and for a goal with
//! nothing to point at "type annotations needed: cannot satisfy `..`" or "cannot normalize
//! `..`"; then why the goal was required ("required by a bound in `f`").
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/infer/
//! need_type_info.rs` (`emit_inference_failure_err`, `bad_inference_failure_err`,
//! `InferenceDiagnosticsData`, `InferSourceKind::ty_localized_msg`, `FindInferSourceVisitor`'s
//! `visit_local`, `expr_inferred_arg_iter` and `source_cost`, `ty_to_string`),
//! `error_reporting/traits/ambiguity.rs` (`maybe_report_ambiguity`,
//! `annotate_source_of_ambiguity`), `error_reporting/traits/mod.rs` (`to_pretty_impl_header`)
//! and `compiler/rustc_trait_selection/src/diagnostics.rs` (`AnnotationRequired`,
//! `InferenceBadError`, `SourceKindSubdiag`, `SpecifyGenericParamsSuggestion`) (MIT or
//! Apache-2.0). The `let` is found from the syntax around where the front end reports it, the
//! path from where the undecided variable was made.
//!
//! The suggested type is rustc's `ty_to_string`: `print::let_type`'s `ClosureEraser` over the
//! type hir names each variable of (`named_term`), when that type is the `let`'s.
//!
//! What the front end does not give yet (requested in `v2/requests/diagnostics-infer2-solver.md`
//! and `v2/requests/db-ty.md`): the named type of a `let` that is not the one writeback found
//! undecided, so an ambiguous goal's `let` (`: Foo<_, &_, W, Z>`) prints every variable `_`;
//! the other places rustc's visitor weighs (every expression whose type holds the variable), so
//! a path is a source only when its item has that one generic parameter; the causes of the
//! related goals; the single impl rustc's E0283 hint puts in the
//! `let`; and rustc's "the following types implement trait" help, E0790 and fully qualified
//! path suggestions.

use either::Either;
use hir::{AsAssocItem as _, InFile, LangItem, PathResolution};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lints::code_fact,
    lower::{Found, Phase, obligation_cause, trait_solver_error, unimplemented_trait::is_lang},
    model::{Applicability, Diag, MultiSpan, Span},
    print,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::TypeMustBeKnown<'db>) -> Vec<Found> {
    let at = d.at_point.map(|it| it.syntax_node_ptr());
    let failure = cx.span_of(at);
    let node = cx.syntax_view(at);
    let code = match d.kind {
        hir::TypeAnnotationNeeded::E0282 => "E0282",
        hir::TypeAnnotationNeeded::E0283 => "E0283",
        hir::TypeAnnotationNeeded::E0284 => "E0284",
    };
    // clone: on the error path, for an error hir found
    let infer = |label| inference_failure(cx, d, code, failure.clone(), node, label);
    let mut diag = match d.ambiguity.as_ref().map(|it| &it.primary) {
        // Where type checking needed it, when it is not the binding: `structurally_resolve_type`
        // labels it, writeback does not.
        None => infer(d.top_term.is_none()),
        // An undecided type that must be sized: the type is what is undecided.
        Some(hir::AmbiguousGoal { goal: hir::SolverGoal::Trait(_), .. }) if d.kind == hir::TypeAnnotationNeeded::E0282 => {
            infer(false)
        }
        Some(hir::AmbiguousGoal { goal: hir::SolverGoal::Trait(pred), candidates }) => {
            let mut diag = match &d.top_term {
                Some(_) => infer(true),
                None => cannot_satisfy(code, &print::predicate(cx, pred), failure, false),
            };
            if candidates.is_empty() {
                diag.note(if pred.trait_ref().self_ty().is_unknown() {
                    // alloc: the error's text, for an error hir found
                    format!("the type must implement `{}`", print::trait_path_sugared(cx, pred))
                } else {
                    // alloc: the error's text, for an error hir found
                    format!("cannot satisfy `{}`", print::predicate(cx, pred))
                });
            } else {
                annotate_source_of_ambiguity(cx, node, &mut diag, &print::predicate(cx, pred), candidates);
            }
            diag
        }
        Some(hir::AmbiguousGoal { goal: hir::SolverGoal::WellFormed(_), .. }) => infer(false),
        Some(hir::AmbiguousGoal {
            goal: hir::SolverGoal::Subtype { .. } | hir::SolverGoal::ConstArgHasType { .. }, ..
        }) => infer(true),
        Some(hir::AmbiguousGoal { goal: hir::SolverGoal::NormalizesTo { alias }, .. }) => {
            let alias = trait_solver_error::term(cx, alias);
            // alloc: the error's text, for an error hir found
            let mut diag = Diag::error(Some(code), format!("type annotations needed: cannot normalize `{alias}`"), failure.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(failure, format!("cannot normalize `{alias}`"));
            diag
        }
        Some(hir::AmbiguousGoal { goal: hir::SolverGoal::UnstableFeature(feature), .. }) => Diag::error(
            None,
            // alloc: the error's text, for an error hir found
            format!(
                "blaze cannot describe the unmet `#[unstable_feature_bound({})]` of an impl yet",
                feature.as_str()
            ),
            failure,
        ),
        Some(hir::AmbiguousGoal { goal, .. }) => match trait_solver_error::goal_text(cx, goal) {
            Some(text) => match (goal, &d.top_term) {
                (hir::SolverGoal::Projection { .. } | hir::SolverGoal::ConstEvaluatable(_), Some(_)) => {
                    let mut diag = infer(true);
                    if matches!(goal, hir::SolverGoal::Projection { .. }) {
                        // alloc: the error's text, for an error hir found
                        diag.note(format!("cannot satisfy `{text}`"));
                    }
                    diag
                }
                _ => cannot_satisfy(code, &text, failure, true),
            },
            None => Diag::error(None, "blaze cannot print the ambiguous requirement", failure),
        },
    };
    // rustc returns before the related goals and the cause for an undecided sized type and an
    // unstable feature bound.
    if let Some(ambiguity) = &d.ambiguity
        && !matches!(ambiguity.primary.goal, hir::SolverGoal::UnstableFeature(_))
        && !(matches!(ambiguity.primary.goal, hir::SolverGoal::Trait(_)) && d.kind == hir::TypeAnnotationNeeded::E0282)
    {
        related_notes(cx, node, &mut diag, &ambiguity.related);
        let trait_ = match &ambiguity.primary.goal {
            hir::SolverGoal::Trait(pred) => Some(pred.trait_ref().trait_()),
            _ => None,
        };
        obligation_cause::note(cx, &mut diag, d.cause, trait_);
    }
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's "type annotations needed: cannot satisfy `goal`", with the label again when `label`.
fn cannot_satisfy(code: &'static str, goal: &str, at: Span, label: bool) -> Diag {
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some(code), format!("type annotations needed: cannot satisfy `{goal}`"), at.clone());
    if label {
        // alloc: the error's text, for an error hir found
        diag.span_label(at, format!("cannot satisfy `{goal}`"));
    }
    diag
}

/// rustc's `emit_inference_failure_err`: the error at the best place to annotate (`let`, or the
/// path whose generic parameter it is), else at where it was found, labelled "type must be known
/// at this point" where that is elsewhere when `label`.
fn inference_failure<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::TypeMustBeKnown<'db>,
    code: &'static str,
    failure: Span,
    node: InFile<ast::Node<'db>>,
    label: bool,
) -> Diag {
    let data = ArgData::of(cx, d);
    let let_source = let_source(cx, node);
    let generic = generic_source(cx, d);
    let (source, collect) = match (let_source, generic) {
        (None, None) => {
            if let (Some(Either::Right(konst)), None) = (&d.top_term, &d.origin) {
                // clone: on the error path, for an error hir found
                let mut diag = Diag::error(Some(code), "type annotations needed", failure.clone());
                // alloc: the error's text, for an error hir found
                diag.span_label(failure, format!("cannot infer the value of the constant `{konst}`"));
                return diag;
            }
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some(code), "type annotations needed", failure.clone());
            diag.span_label(failure, data.bad_label());
            return diag;
        }
        (Some(source), None) => (Either::Left(source), false),
        (None, Some(generic)) => (Either::Right(generic), false),
        (Some(source), Some(generic)) => {
            let let_cost = ty_cost(cx, &source.ty);
            let in_init = source.stmt.initializer().is_some_and(|it| it.syntax().text_range().contains_range(generic.range));
            let generic_first = in_init || generic.range.start() < source.stmt.syntax().text_range().start();
            if generic_first {
                if let_cost + 1 < generic.cost {
                    let collect = source.ty.is_unknown() && is_collect(cx, source.stmt);
                    (Either::Left(source), collect)
                } else {
                    (Either::Right(generic), false)
                }
            } else if generic.cost + 1 < let_cost {
                (Either::Right(generic), false)
            } else {
                (Either::Left(source), false)
            }
        }
    };
    match source {
        Either::Left(source) => {
            let named = let_named(cx, d, node, &source);
            let_failure(cx, &data, code, failure, label, source, named, collect)
        }
        Either::Right(generic) => {
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some(code), "type annotations needed", generic.span.clone());
            if label && !overlaps(&failure, &generic.span) {
                diag.span_label(failure, "type must be known at this point");
            }
            let (what, kind) = if generic.is_type { ("type", "type") } else { ("the value", "const") };
            let param = match &data.parent {
                // alloc: the error's text, for an error hir found
                Some((prefix, name)) => format!("parameter `{}` declared on the {prefix} `{name}`", generic.param),
                // alloc: the error's text, for an error hir found
                None => format!("parameter {}", generic.param),
            };
            // alloc: the error's text, for an error hir found
            diag.span_label(generic.span, format!("cannot infer {what} of the {kind} {param}"));
            if !generic.turbofish {
                let (message, code) = if generic.is_type {
                    // alloc: the error's text, for an error hir found
                    (format!("consider specifying a concrete type for the type parameter `{}`", generic.param), "::</* Type */>")
                } else {
                    // alloc: the error's text, for an error hir found
                    (format!("consider specifying a const for the const parameter `{}`", generic.param), "::</* CONST */>")
                };
                diag.suggestion_verbose(generic.insert, message, code, Applicability::HasPlaceholders);
            }
            diag
        }
    }
}

/// The `let`'s type with each variable made for a parameter named by it, where hir gives it:
/// `named_term` when the type writeback found undecided is the `let`'s, or that of the function
/// item the initializer calls, at whose path the variable was made, when the call's type is the
/// `let`'s. For an ambiguous goal `top_term` is the goal's variable alone, and hir does not say
/// how the `let`'s other variables are named (asked in `v2/requests/db-ty.md`), so none is.
fn let_named<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::TypeMustBeKnown<'db>,
    node: InFile<ast::Node<'db>>,
    source: &LetSource<'db>,
) -> Option<hir::Type<'db>> {
    let (Some(Either::Left(top)), Some(Either::Left(named))) = (&d.top_term, &d.named_term) else {
        return None;
    };
    if top.to_next_solver() == source.ty.to_next_solver() {
        return Some(*named);
    }
    let Some(ast::Expr::CallExpr(call)) = source.stmt.initializer() else { return None };
    let callee = call.expr()?;
    if node.file_id != source.pat.file_id || callee.syntax().text_range() != node.value.text_range() {
        return None;
    }
    let returns = |ty: &hir::Type<'db>| {
        let callable = ty.as_callable(cx.db)?;
        matches!(
            callable.kind(),
            hir::CallableKind::Function(_) | hir::CallableKind::TupleStruct(_) | hir::CallableKind::TupleEnumVariant(_)
        )
        .then(|| callable.return_type())
    };
    let output = returns(top)?;
    (output.to_next_solver() == source.ty.to_next_solver()).then(|| returns(named)).flatten()
}

/// The error at a `let`: rustc's `InferSourceKind::LetBinding` and `SourceKindSubdiag::LetLike`.
fn let_failure<'db>(
    cx: &Cx<'_, 'db>,
    data: &ArgData,
    code: &'static str,
    failure: Span,
    label: bool,
    source: LetSource<'db>,
    named: Option<hir::Type<'db>>,
    collect: bool,
) -> Diag {
    let span = cx.span(source.pat.map(|it| it.syntax().text_range()));
    let printed = cx.ty(&source.ty);
    let message = if names_type(&source.ty) {
        // alloc: the error's text, for an error hir found
        format!("type annotations needed for `{}`", placeholders(&printed))
    } else {
        // alloc: the error's text, for an error hir found
        "type annotations needed".to_owned()
    };
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some(code), message, span.clone());
    if label && !overlaps(&failure, &span) {
        diag.span_label(failure, "type must be known at this point");
    }
    let name = match source.pat.value {
        ast::Pat::IdentPat(it) => it.name().map(|it| it.text()),
        _ => None,
    };
    let message = match name {
        // alloc: the error's text, for an error hir found
        Some(name) => format!("consider giving `{name}` an explicit type{}", data.where_specified(cx, &source.ty)),
        // alloc: the error's text, for an error hir found
        None => format!("consider giving this pattern a type{}", data.where_specified(cx, &source.ty)),
    };
    let type_name = if collect {
        // alloc: the error's text, for an error hir found
        "Vec<_>".to_owned()
    } else if source.ty.is_unknown() {
        // alloc: the error's text, for an error hir found
        "/* Type */".to_owned()
    } else {
        print::let_type(cx, &source.ty, named.as_ref())
    };
    diag.suggestion_verbose(
        span.shrink_to_hi(),
        message,
        // alloc: the error's text, for an error hir found
        format!(": {type_name}"),
        Applicability::HasPlaceholders,
    );
    diag
}

/// What is undecided, as rustc's `InferenceDiagnosticsData` names it: the type or const
/// parameter the variable was made for, else the type or constant printed.
struct ArgData {
    name: String,
    kind: ArgKind,
    /// The item the parameter is declared on: its `def_descr` and name.
    parent: Option<(&'static str, String)>,
}

enum ArgKind {
    Type { prefix: String },
    ConstParam,
    Const,
}

impl ArgData {
    fn of<'db>(cx: &Cx<'_, 'db>, d: &hir::TypeMustBeKnown<'db>) -> ArgData {
        let is_variable = match &d.top_term {
            None | Some(Either::Right(_)) => true,
            Some(Either::Left(ty)) => ty.is_unknown(),
        };
        if is_variable && let Some((_, param)) = &d.origin {
            let parent = descr(cx, param.parent());
            match param {
                hir::GenericParam::TypeParam(it) => {
                    return ArgData {
                        // alloc: the error's text, for an error hir found
                        name: it.name(cx.db).as_str().to_owned(),
                        // alloc: the error's text, for an error hir found
                        kind: ArgKind::Type { prefix: "type parameter".to_owned() },
                        parent,
                    };
                }
                hir::GenericParam::ConstParam(it) => {
                    // alloc: the error's text, for an error hir found
                    return ArgData { name: it.name(cx.db).as_str().to_owned(), kind: ArgKind::ConstParam, parent };
                }
                hir::GenericParam::LifetimeParam(_) => {}
            }
        }
        match &d.top_term {
            Some(Either::Left(ty)) if !ty.is_unknown() => ArgData {
                name: placeholders(&cx.ty(ty)),
                kind: ArgKind::Type { prefix: print::prefix_string(cx, ty) },
                parent: None,
            },
            // clone: on the error path, for an error hir found
            Some(Either::Right(ct)) => ArgData { name: ct.clone(), kind: ArgKind::Const, parent: None },
            _ => ArgData {
                // alloc: the error's text, for an error hir found
                name: "_".to_owned(),
                // alloc: the error's text, for an error hir found
                kind: ArgKind::Type { prefix: String::new() },
                parent: None,
            },
        }
    }

    /// rustc's `InferenceBadError` label: "cannot infer type", "cannot infer type for type
    /// parameter `T` declared on the function `f`".
    fn bad_label(&self) -> String {
        if self.name == "_" && matches!(self.kind, ArgKind::Type { .. }) {
            // alloc: the error's text, for an error hir found
            return "cannot infer type".to_owned();
        }
        let what = match &self.kind {
            // alloc: the error's text, for an error hir found
            ArgKind::Type { prefix } => format!("type for {prefix}"),
            // alloc: the error's text, for an error hir found
            ArgKind::ConstParam => "the value of const parameter".to_owned(),
            // alloc: the error's text, for an error hir found
            ArgKind::Const => "the value of the constant".to_owned(),
        };
        let parent = match &self.parent {
            // alloc: the error's text, for an error hir found
            Some((prefix, name)) => format!(" declared on the {prefix} `{name}`"),
            // alloc: the error's text, for an error hir found
            None => String::new(),
        };
        // alloc: the error's text, for an error hir found
        format!("cannot infer {what} `{}`{parent}", self.name)
    }

    /// rustc's `where_x_is_kind` text after "consider giving `x` an explicit type" for a `let` of
    /// type `ty`.
    fn where_specified<'db>(&self, cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> String {
        if ty.is_unknown() {
            // alloc: the error's text, for an error hir found
            return String::new();
        }
        if self.name == "_" {
            return match print::let_placeholders(cx, ty) {
                // alloc: the error's text, for an error hir found
                0 => String::new(),
                // alloc: the error's text, for an error hir found
                1 => ", where the placeholder `_` is specified".to_owned(),
                // alloc: the error's text, for an error hir found
                _ => ", where the placeholders `_` are specified".to_owned(),
            };
        }
        let what = match &self.kind {
            // alloc: the error's text, for an error hir found
            ArgKind::Type { prefix } => format!("type for {prefix}"),
            // alloc: the error's text, for an error hir found
            ArgKind::ConstParam => "value of const parameter".to_owned(),
            // alloc: the error's text, for an error hir found
            ArgKind::Const => "value of the constant".to_owned(),
        };
        // alloc: the error's text, for an error hir found
        format!(", where the {what} `{}` is specified", self.name)
    }
}

/// rustc's `InferenceDiagnosticsParentData`: the `def_descr` and name of the item a generic
/// parameter is declared on; `None` for an impl, which has no name.
fn descr(cx: &Cx<'_, '_>, item: hir::GenericDef) -> Option<(&'static str, String)> {
    let (prefix, name) = match item {
        hir::GenericDef::Function(it) => {
            let prefix = if it.has_self_param(cx.db) {
                "method"
            } else if it.as_assoc_item(cx.db).is_some() {
                "associated function"
            } else {
                "function"
            };
            (prefix, it.name(cx.db))
        }
        hir::GenericDef::Adt(hir::Adt::Struct(it)) => ("struct", it.name(cx.db)),
        hir::GenericDef::Adt(hir::Adt::Enum(it)) => ("enum", it.name(cx.db)),
        hir::GenericDef::Adt(hir::Adt::Union(it)) => ("union", it.name(cx.db)),
        hir::GenericDef::Trait(it) => ("trait", it.name(cx.db)),
        hir::GenericDef::TypeAlias(it) => {
            (if it.as_assoc_item(cx.db).is_some() { "associated type" } else { "type alias" }, it.name(cx.db))
        }
        hir::GenericDef::Const(it) => {
            (if it.as_assoc_item(cx.db).is_some() { "associated constant" } else { "constant" }, it.name(cx.db)?)
        }
        hir::GenericDef::Static(it) => ("static", it.name(cx.db)),
        hir::GenericDef::Impl(_) => return None,
    };
    // alloc: the error's text, for an error hir found
    Some((prefix, name.as_str().to_owned()))
}

/// The path, method call or struct expression the undecided variable was made at, when its item
/// has that one generic parameter: rustc's `InferSourceKind::GenericArg`.
struct GenericSource {
    /// The expression, or the method's name.
    span: Span,
    /// After the name, to the end of its segment: where `::<..>` goes.
    insert: Span,
    range: TextRange,
    turbofish: bool,
    is_type: bool,
    param: String,
    /// rustc's `source_cost`: 15 of an enum variant's path, else 10.
    cost: usize,
}

fn generic_source<'db>(cx: &Cx<'_, 'db>, d: &hir::TypeMustBeKnown<'db>) -> Option<GenericSource> {
    let (src, param) = d.origin.as_ref()?;
    let (is_type, name) = match param {
        hir::GenericParam::TypeParam(it) => (true, it.name(cx.db)),
        hir::GenericParam::ConstParam(it) => (false, it.name(cx.db)),
        hir::GenericParam::LifetimeParam(_) => return None,
    };
    let item = param.parent();
    // rustc's cost adds each other argument's, which the front end does not give; an `impl
    // Trait` parameter (rustc gives no source then) is one more parameter.
    if item.type_or_const_params(cx.db).len() != 1 {
        return None;
    }
    let node = cx.syntax_view(src.map(|it| it.syntax_node_ptr()));
    if node.file_id.is_macro() {
        return None;
    }
    let in_file = |range: TextRange| cx.span(node.with_value(range));
    let range = node.value.text_range();
    let (span, name_end, end, args, cost) = if let Some(call) = ast::MethodCallExpr::cast(node.value) {
        let method = cx.vsema.resolve_method_call(call)?;
        if item != hir::GenericDef::Function(method) {
            return None;
        }
        let name_ref = call.name_ref()?;
        let args = call.generic_arg_list();
        let end = args.map_or(name_ref.syntax().text_range().end(), |it| it.syntax().text_range().end());
        (in_file(name_ref.syntax().text_range()), name_ref.syntax().text_range().end(), end, args, 10)
    } else {
        let path = match ast::Expr::cast(node.value)? {
            ast::Expr::PathExpr(it) => it.path()?,
            ast::Expr::RecordExpr(it) => it.path()?,
            _ => return None,
        };
        let is_record = ast::RecordExpr::can_cast(node.value.kind());
        // alloc: the error's parts, for an error hir found
        let mut segments = Vec::new();
        let mut at = Some(path);
        while let Some(path) = at {
            segments.push(path);
            at = path.qualifier();
        }
        segments.iter().rev().enumerate().find_map(|(index, path)| {
            let (def, cost) = match cx.vsema.resolve_path(*path)? {
                PathResolution::Def(hir::ModuleDef::Function(it)) if !is_record => (hir::GenericDef::Function(it), 10),
                PathResolution::Def(hir::ModuleDef::Adt(it)) => (hir::GenericDef::Adt(it), 10),
                PathResolution::Def(hir::ModuleDef::EnumVariant(it)) if !is_record => {
                    (hir::GenericDef::Adt(hir::Adt::Enum(it.parent_enum(cx.db))), 15)
                }
                _ => return None,
            };
            if def != item {
                return None;
            }
            let segment = path.segment()?;
            let name_ref = segment.name_ref()?;
            Some((
                in_file(range),
                name_ref.syntax().text_range().end(),
                segment.syntax().text_range().end(),
                segment.generic_arg_list(),
                cost + index,
            ))
        })?
    };
    let turbofish = args.is_some_and(|it| it.generic_args().any(|arg| !matches!(arg, ast::GenericArg::LifetimeArg(_))));
    Some(GenericSource {
        span,
        insert: in_file(TextRange::new(name_end, end)),
        range,
        turbofish,
        is_type,
        // alloc: the error's text, for an error hir found
        param: name.as_str().to_owned(),
        cost,
    })
}

/// rustc's `source_cost` of a `let`'s type.
fn ty_cost<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> usize {
    if ty.is_closure() {
        return 1000;
    }
    if let Some(callable) = ty.as_callable(cx.db)
        && !matches!(callable.kind(), hir::CallableKind::Closure(_))
        && ty.is_fn()
    {
        return if matches!(callable.kind(), hir::CallableKind::FnPtr) { 30 } else { 150 };
    }
    if let Some(adt) = ty.as_adt() {
        let defaults: Vec<_> = hir::GenericDef::Adt(adt)
            .type_or_const_params(cx.db)
            .into_iter()
            .filter_map(|it| it.as_type_param(cx.db))
            .map(|it| it.default(cx.db))
            // alloc: the error's parts, for an error hir found
            .collect();
        // alloc: the error's parts, for an error hir found
        let mut args: Vec<_> = ty.type_arguments().collect();
        while let Some(last) = args.last()
            && defaults.get(args.len() - 1).is_some_and(|it| it.as_ref() == Some(last))
        {
            args.pop();
        }
        return 5 + args.iter().map(|it| ty_cost(cx, it)).sum::<usize>();
    }
    if ty.is_tuple() {
        return 5 + ty.tuple_fields(cx.db).iter().map(|it| ty_cost(cx, it)).sum::<usize>();
    }
    if let Some((inner, _)) = ty.as_reference() {
        return 2 + ty_cost(cx, &inner);
    }
    if ty.is_unknown() { 0 } else { 1 }
}

/// Whether a `let`'s initializer is a call of `Iterator::collect`, whose undecided type rustc
/// suggests as `Vec<_>`.
fn is_collect<'db>(cx: &Cx<'_, 'db>, stmt: ast::LetStmt<'db>) -> bool {
    let function = match stmt.initializer() {
        Some(ast::Expr::MethodCallExpr(call)) => cx.vsema.resolve_method_call(call),
        Some(ast::Expr::CallExpr(call)) => match call.expr() {
            Some(ast::Expr::PathExpr(path)) => match path.path().and_then(|it| cx.vsema.resolve_path(it)) {
                Some(PathResolution::Def(hir::ModuleDef::Function(it))) => Some(it),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    };
    function.is_some_and(|it| {
        it.name(cx.db).as_str() == "collect"
            && it.as_assoc_item(cx.db).and_then(|it| it.container_trait(cx.db)).is_some_and(|tr| is_lang(cx, tr, LangItem::Iterator))
    })
}

/// rustc's notes of the goals merged into the error, the same variable's at the same place:
/// "the type must also implement `Trait`", "cannot satisfy `..`" or the impls that could meet it,
/// each once.
fn related_notes<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, diag: &mut Diag, related: &[hir::AmbiguousGoal<'db>]) {
    // alloc: the error's parts, for an error hir found
    let mut mentioned: Vec<String> = Vec::new();
    for goal in related {
        let note = match &goal.goal {
            hir::SolverGoal::Trait(pred)
                if ![LangItem::Sized, LangItem::MetaSized, LangItem::PointeeSized]
                    .into_iter()
                    .any(|it| is_lang(cx, pred.trait_ref().trait_(), it)) =>
            {
                if pred.trait_ref().self_ty().is_unknown() {
                    // alloc: the error's text, for an error hir found
                    format!("the type must also implement `{}`", print::trait_path_sugared(cx, pred))
                } else {
                    let text = print::predicate(cx, pred);
                    // alloc: the error's text, for an error hir found
                    let note = format!("cannot satisfy `{text}`");
                    if !mentioned.contains(&note) && !goal.candidates.is_empty() {
                        annotate_source_of_ambiguity(cx, node, diag, &text, &goal.candidates);
                        mentioned.push(note);
                        continue;
                    }
                    note
                }
            }
            hir::SolverGoal::Projection { .. } => match trait_solver_error::goal_text(cx, &goal.goal) {
                // alloc: the error's text, for an error hir found
                Some(text) => format!("cannot satisfy `{text}`"),
                None => continue,
            },
            _ => continue,
        };
        if !mentioned.contains(&note) {
            // clone: on the error path, for an error hir found
            diag.note(note.clone());
            mentioned.push(note);
        }
    }
}

/// rustc's `annotate_source_of_ambiguity`: "multiple `impl`s satisfying `pred` found", on the
/// local impls and the where clauses, and naming the crates of the others with their headers.
fn annotate_source_of_ambiguity<'db>(
    cx: &Cx<'_, 'db>,
    node: InFile<ast::Node<'db>>,
    diag: &mut Diag,
    pred: &str,
    candidates: &[hir::AmbiguityCandidate<'db>],
) {
    // alloc: the error's parts, for an error hir found
    let mut spans = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut crates: Vec<String> = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut post: Vec<String> = Vec::new();
    let mut has_param_env = false;
    // alloc: the error's parts, for an error hir found
    let mut where_clauses = Vec::new();
    for candidate in candidates {
        match candidate {
            hir::AmbiguityCandidate::Impl(it) => {
                let krate = it.module(cx.db).krate(cx.db);
                if krate == cx.krate {
                    let Some(src) = cx.vsema.source(*it) else { continue };
                    spans.push(cx.span(src.map(|it| def_range(it.syntax()))));
                } else {
                    crates.push(crate_name(cx, krate));
                    if let Some(header) = impl_header(cx, *it) {
                        post.push(header);
                    }
                }
            }
            hir::AmbiguityCandidate::WhereClause(tr) => {
                has_param_env = true;
                where_clauses.push(tr.trait_());
            }
        }
    }
    if !where_clauses.is_empty() {
        match where_clause_spans(cx, node, &where_clauses) {
            Some(found) => spans.extend(found),
            None => {
                diag.note("blaze cannot find the `where` clauses this requirement is ambiguous between");
                return;
            }
        }
    }
    // alloc: the error's parts, for an error hir found
    let mut crate_names: Vec<String> = crates.iter().map(|it| format!("`{it}`")).collect();
    crate_names.sort();
    crate_names.dedup();
    post.sort();
    post.dedup();
    let or_where = if has_param_env { " or `where` clauses" } else { "" };
    // alloc: the error's text, for an error hir found
    let msg = format!("multiple `impl`s{or_where} satisfying `{pred}` found");
    let post = if post.len() > 1 || post.first().is_some_and(|it| it.contains('\n')) {
        // alloc: the error's text, for an error hir found
        format!(":\n{}", post.iter().map(|it| format!("- {it}")).collect::<Vec<_>>().join("\n"))
    } else if let Some(only) = post.first() {
        // alloc: the error's text, for an error hir found
        format!(": `{only}`")
    } else {
        // alloc: the error's text, for an error hir found
        String::new()
    };
    // clone: on the error path, for an error hir found
    let multi = || MultiSpan { primary: spans.clone(), labels: Vec::new() };
    match (spans.len(), crates.len(), crate_names.len()) {
        (0, 0, 0) => {
            // alloc: the error's text, for an error hir found
            diag.note(format!("cannot satisfy `{pred}`"));
        }
        (0, _, 1) => {
            // alloc: the error's text, for an error hir found
            diag.note(format!("{msg} in the `{}` crate{post}", crates[0]));
        }
        (0, _, _) => {
            // alloc: the error's text, for an error hir found
            diag.note(format!("{msg} in the following crates: {}{post}", crate_names.join(", ")));
        }
        (_, 0, 0) => {
            diag.span_note(multi(), msg);
        }
        (_, 1, 1) => {
            diag.span_note(multi(), msg);
            // alloc: the error's text, for an error hir found
            diag.note(format!("and another `impl` found in the `{}` crate{post}", crates[0]));
        }
        _ => {
            diag.span_note(multi(), msg);
            // alloc: the error's text, for an error hir found
            diag.note(format!("and more `impl`s found in the following crates: {}{post}", crate_names.join(", ")));
        }
    }
}

/// rustc's `tcx.crate_name`: the crate's name, else its root file's stem.
fn crate_name(cx: &Cx<'_, '_>, krate: hir::Crate) -> String {
    match krate.display_name(cx.db) {
        Some(name) => name.crate_name().to_string(), // alloc: the error's text, for an error hir found
        None => {
            let path = cx.file_name(krate.root_file(cx.db));
            let stem = path.rsplit('/').next().unwrap_or(&path);
            stem.strip_suffix(".rs").unwrap_or(stem).replace('-', "_")
        }
    }
}

/// rustc's `to_pretty_impl_header`: `impl<T> Trait<U> for S<T>` with its where clauses and `;`.
/// `None` for an impl with a where clause that is not a trait's, which blaze does not print yet.
fn impl_header(cx: &Cx<'_, '_>, impl_: hir::Impl) -> Option<String> {
    let tr = impl_.trait_ref(cx.db)?;
    let def = hir::GenericDef::Impl(impl_);
    // alloc: the error's text, for an error hir found
    let mut w = "impl".to_owned();
    // alloc: the error's text, for an error hir found
    let names: Vec<String> = def
        .lifetime_params(cx.db)
        .into_iter()
        // alloc: the error's text, for an error hir found
        .map(|it| it.name(cx.db).as_str().to_owned())
        .filter(|it| it != "'_")
        // alloc: the error's text, for an error hir found
        .chain(def.type_or_const_params(cx.db).into_iter().map(|it| it.name(cx.db).as_str().to_owned()))
        // alloc: the error's text, for an error hir found
        .collect();
    if !names.is_empty() {
        w.push('<');
        w.push_str(&names.join(", "));
        w.push('>');
    }
    w.push(' ');
    if impl_.is_negative(cx.db) {
        w.push('!');
    }
    w.push_str(&print::trait_path(cx, &tr));
    w.push_str(" for ");
    w.push_str(&cx.ty(&impl_.self_ty(cx.db)));
    // alloc: the error's text, for an error hir found
    let mut clauses = Vec::new();
    for pred in def.where_predicates(cx.db) {
        let clause = pred.as_trait()?;
        if is_lang(cx, clause.trait_ref().trait_(), LangItem::Sized) {
            continue;
        }
        clauses.push(print::predicate(cx, &clause));
    }
    if !clauses.is_empty() {
        w.push_str("\n  where ");
        w.push_str(&clauses.join(", "));
    }
    w.push(';');
    Some(w)
}

/// The where clauses rustc points at for an ambiguity between them: the bounds of each trait
/// written on the item the error is in and on its impl or trait, when they are as many as the
/// candidates.
fn where_clause_spans<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>, traits: &[hir::Trait]) -> Option<Vec<Span>> {
    // alloc: the error's parts, for an error hir found
    let mut spans = Vec::new();
    for (index, &trait_) in traits.iter().enumerate() {
        if traits[..index].contains(&trait_) {
            continue;
        }
        let count = traits.iter().filter(|it| **it == trait_).count();
        // alloc: the error's parts, for an error hir found
        let mut found = Vec::new();
        for ancestor in cx.vsema.ancestors_with_macros_file(node) {
            let Some(generic) = ast::AnyHasGenericParams::cast(ancestor.value) else { continue };
            let wheres = generic.where_clause();
            for bound in bounds(generic.generic_param_list(), wheres) {
                if let Some(ast::Type::PathType(ty)) = bound.ty()
                    && let Some(PathResolution::Def(hir::ModuleDef::Trait(it))) =
                        ty.path().and_then(|it| cx.vsema.resolve_path(it))
                    && it == trait_
                {
                    found.push(cx.span(ancestor.with_value(bound.syntax().text_range())));
                }
            }
        }
        if found.len() != count {
            return None;
        }
        spans.extend(found);
    }
    Some(spans)
}

/// The bounds written on an item's type parameters and in its where clause.
fn bounds<'t>(
    params: Option<ast::GenericParamList<'t>>,
    wheres: Option<ast::WhereClause<'t>>,
) -> impl Iterator<Item = ast::TypeBound<'t>> {
    let params = params.into_iter().flat_map(|it| it.type_or_const_params()).filter_map(|param| match param {
        ast::TypeOrConstParam::Type(param) => param.type_bound_list(),
        ast::TypeOrConstParam::Const(_) => None,
    });
    let wheres = wheres.into_iter().flat_map(|it| it.predicates()).filter_map(|pred| pred.type_bound_list());
    params.chain(wheres).flat_map(|list| list.bounds())
}

/// A `let` with no type whose pattern's type is not decided: rustc's `InferSourceKind::
/// LetBinding`.
struct LetSource<'db> {
    pat: InFile<ast::Pat<'db>>,
    ty: hir::Type<'db>,
    stmt: ast::LetStmt<'db>,
}

/// The `let` the undecided type at `node` is of: the binding itself, the local an expression
/// names, or the `let` whose initializer `node` is in.
fn let_source<'db>(cx: &Cx<'_, 'db>, node: InFile<ast::Node<'db>>) -> Option<LetSource<'db>> {
    let of_stmt = |stmt: ast::LetStmt<'db>, file| {
        if stmt.ty().is_some() {
            return None;
        }
        let pat = stmt.pat()?;
        let ty = cx.vsema.type_of_pat(pat)?.original;
        ty.contains_unknown().then(|| LetSource { pat: InFile::new(file, pat), ty, stmt })
    };
    if ast::Pat::can_cast(node.value.kind())
        && let Some(stmt) = node.value.parent().and_then(ast::LetStmt::cast)
    {
        return of_stmt(stmt, node.file_id);
    }
    if let Some(ast::Expr::PathExpr(path)) = ast::Expr::cast(node.value)
        && let Some(PathResolution::Local(local)) = path.path().and_then(|it| cx.vsema.resolve_path(it))
    {
        let source = local.primary_source_view(cx.db);
        // The local's tree is given out by `vsema`, so `of_stmt` may ask it of the pattern.
        cx.root(source.file());
        if let Some(stmt) = source.syntax().parent().and_then(ast::LetStmt::cast)
            && let Some(found) = of_stmt(stmt, source.file())
        {
            return Some(found);
        }
    }
    // The node the walk came from is a child of the next, in its file.
    let mut prev = node.value;
    for ancestor in cx.vsema.ancestors_with_macros_file(node).skip(1) {
        let kind = ancestor.value.kind();
        if ast::Item::can_cast(kind) || ast::ClosureExpr::can_cast(kind) {
            break;
        }
        if let Some(stmt) = ast::LetStmt::cast(ancestor.value) {
            let in_init = stmt
                .initializer()
                .is_some_and(|init| init.syntax().text_range().contains_range(prev.text_range()));
            return if in_init { of_stmt(stmt, ancestor.file_id) } else { None };
        }
        prev = ancestor.value;
    }
    None
}

/// rustc's `ty_localized_msg` "normal": whether the message names the type, which it does not
/// of `_`, a primitive, or an ADT with no type or const arguments.
fn names_type(ty: &hir::Type<'_>) -> bool {
    if ty.is_unknown() || ty.as_builtin().is_some() || ty.is_unit() || ty.is_never() {
        return false;
    }
    match ty.as_adt() {
        Some(_) => ty.type_arguments().next().is_some(),
        None => true,
    }
}

/// rustc's `ty_to_string`: the type with what is not decided as `_`.
fn placeholders(printed: &str) -> String {
    printed.replace("{unknown}", "_").replace("{type error}", "_")
}

pub(super) fn overlaps(a: &crate::model::Span, b: &crate::model::Span) -> bool {
    a.file == b.file && a.lo < b.hi && b.lo < a.hi
}
