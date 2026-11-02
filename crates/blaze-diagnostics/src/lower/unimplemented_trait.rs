//! E0277: a trait bound that does not hold, in rustc's words: the message, label and notes of
//! the trait's `#[rustc_on_unimplemented]` or `#[diagnostic::on_unimplemented]` (read from the
//! trait's attributes, `{Self}`, `{This}` and the trait's generic names filled in), else "the
//! trait bound `T: Trait` is not satisfied" and "the trait `Trait` is not implemented for `T`";
//! "consider annotating `S` with `#[derive(Debug)]`" for a local type and a derivable trait;
//! "this trait has no implementations"; the "required for `X` to implement `Y`" chain of the
//! impls that needed the bound, on a local impl's header with "unsatisfied trait bound
//! introduced here" on its bound; and the obligation cause's notes (`obligation_cause.rs`:
//! "required by a bound in `f`" on the callee's bound), the error pointed at the one argument
//! the bound is of, as rustc's typeck adjusts it.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/
//! fulfillment_errors.rs` (`report_selection_error`, `get_standard_error_message`,
//! `try_to_add_help_message`), `suggestions.rs` (`note_obligation_cause_code`,
//! `get_explanation_based_on_obligation`, `can_suggest_derive`, `suggest_derive`),
//! `on_unimplemented.rs` (`OnUnimplementedDirective::evaluate`, the format string's
//! arguments), and `compiler/rustc_hir_typeck/src/fn_ctxt/adjust_fulfillment_errors.rs`
//! (`blame_specific_arg_if_possible`) (MIT or Apache-2.0).
//!
//! What the front end does not give yet (requested in `v2/requests/diagnostics.md` and
//! `v2/requests/diagnostics-infer2-solver.md`): the span of a derive's impl, and whether a span
//! is of a format string's desugaring.
//!
//! Cost: nothing until the solver reports the error. Then O(the call's arguments, the
//! callee's bounds and the trait's attributes): types compare as interned ids, never printed;
//! whether the trait has impls is read from its row of the cached impl tables; the
//! `on_unimplemented` arguments are made only for a trait with the attribute.

use base_db::{CrateOrigin, LangCrateOrigin};
use hir::{InFile, LangItem};
use hir_ty::method_resolution::TraitImpls;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, AstToken as _, HasArgList as _, HasAttrs as _, HasGenericArgs as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lints::code_fact,
    lower::{Found, Phase, obligation_cause},
    model::{Applicability, Diag, MultiSpan, Span},
    print,
};

/// rustc's `diagnostic_width`, past which an explanation is a help and not a label.
const DIAGNOSTIC_WIDTH: usize = 140;

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::UnimplementedTrait<'db>) -> Vec<Found> {
    let leaf = &d.trait_predicate;
    let root = d.parent_trait_predicates.last().unwrap_or(leaf);
    let leaf_ref = leaf.trait_ref();
    let leaf_trait = leaf_ref.trait_();
    let leaf_self = leaf_ref.get_type_argument(0);
    let root_ref = root.trait_ref();
    let root_self = root_ref.get_type_argument(0);
    let main = match (&leaf_self, &root_self) {
        (Some(leaf_self), Some(root_self))
            if root_ref.trait_() != leaf_trait
                && !is_lang(cx, root_ref.trait_(), LangItem::Unsize)
                && (*leaf_self == root_self.strip_references() || leaf_self == root_self) =>
        {
            root
        }
        _ => leaf,
    };
    let main_is_leaf = std::ptr::eq(main, leaf);
    let direct = d.parent_trait_predicates.is_empty() || !main_is_leaf;

    let at = cx.syntax_view(d.span.map(|it| it.syntax_node_ptr()));
    let cause = Cause::of(cx, at, |ty| {
        root_self.as_ref().is_some_and(|want| ty == want || ty.strip_references() == *want)
    });
    let custom = on_unimplemented(cx, main, direct);
    // clone: on the error path, for an error hir found
    let message = custom.message.clone().unwrap_or_else(|| {
        // alloc: the error's text, for an error hir found
        format!("the trait bound `{}` is not satisfied", print::predicate(cx, main))
    });
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0277"), message, cause.span.clone());

    let explanation = explanation(cx, leaf);
    let derivable = can_suggest_derive(cx, leaf);
    let ty_span = match leaf_self.as_ref().and_then(|it| it.as_adt()) {
        Some(adt) if is_local_adt(cx, adt) && !derivable => adt_def_span(cx, adt),
        _ => None,
    };
    match &custom.label {
        Some(label) => {
            // clone: on the error path, for an error hir found
            diag.span_label(cause.span.clone(), label.clone());
            let is_param = leaf_self.as_ref().is_some_and(|it| it.as_type_param(cx.db).is_some());
            if !is_param {
                // clone: on the error path, for an error hir found
                match ty_span.clone() {
                    Some(at) => diag.span_help(at, explanation),
                    None => diag.help(explanation),
                };
            }
        }
        None if explanation.len() > DIAGNOSTIC_WIDTH || ty_span.is_some() => {
            // clone: on the error path, for an error hir found
            diag.span_label(cause.span.clone(), "unsatisfied trait bound");
            // clone: on the error path, for an error hir found
            match ty_span.clone() {
                Some(at) => diag.span_help(at, explanation),
                None => diag.help(explanation),
            };
        }
        None => {
            // clone: on the error path, for an error hir found
            diag.span_label(cause.span.clone(), explanation);
        }
    }
    if !(main_is_leaf && derivable) {
        for note in &custom.notes {
            // clone: on the error path, for an error hir found
            diag.note(note.clone());
        }
    }
    cause.label_call(&mut diag);
    if derivable {
        suggest_derive(cx, leaf, &mut diag);
    }
    if is_local_trait(cx, leaf_trait)
        && !has_impls(cx, leaf_trait)
        && !leaf_trait.is_auto(cx.db)
        && leaf.polarity() == hir::ClausePolarity::Positive
        && let Some(src) = cx.vsema.source(leaf_trait)
    {
        let range = src.map(|it| def_range(it.syntax()));
        diag.span_help(cx.span(range), "this trait has no implementations, consider adding one");
    }
    note_derived(cx, d, &mut diag);
    obligation_cause::note(cx, &mut diag, d.cause, Some(root_ref.trait_()));

    for pred in std::iter::once(leaf).chain(&d.parent_trait_predicates) {
        let tr = pred.trait_ref();
        if let Some(self_ty) = tr.get_type_argument(0) {
            // alloc: the error's text, for an error hir found
            let what = format!(
                "whether `{}` implements `{}`",
                cx.ty(&self_ty),
                print::trait_path(cx, &tr)
            );
            diag.fact_opt(cx.implements_fact(tr.trait_(), &self_ty), what);
        }
    }
    code_fact(cx, &mut diag, d.span.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `get_explanation_based_on_obligation`: "the trait `Trait` is not implemented for
/// `T`".
fn explanation<'db>(cx: &Cx<'_, 'db>, pred: &hir::TraitClause<'db>) -> String {
    let tr = pred.trait_ref();
    if pred.polarity() == hir::ClausePolarity::Negative {
        // alloc: the error's text, for an error hir found
        return format!("the trait bound `{}` is not satisfied", print::predicate(cx, pred));
    }
    let self_ty = tr.get_type_argument(0);
    let desc = match &self_ty {
        Some(ty)
            if ty
                .as_callable(cx.db)
                .is_some_and(|it| matches!(it.kind(), hir::CallableKind::Function(_))) =>
        {
            " fn item"
        }
        Some(ty) if ty.is_closure() => " closure",
        _ => "",
    };
    let unstable = if is_unstable(cx, tr.trait_()) { "nightly-only, unstable " } else { "" };
    let self_text = self_ty.as_ref().map(|it| cx.ty(it)).unwrap_or_default();
    // alloc: the error's text, for an error hir found
    format!(
        "the {unstable}trait `{}` is not implemented for{desc} `{self_text}`",
        print::trait_path_sugared(cx, pred)
    )
}

/// Where rustc points a bound of a call that does not hold: at the one argument whose type
/// the bound is of, the callee then labelled "required by a bound introduced by this call";
/// else where the bound was registered.
pub(super) struct Cause {
    pub(super) span: Span,
    /// The span of the callee, when the error was moved to an argument.
    pub(super) callee_span: Option<Span>,
}

impl Cause {
    /// The cause of a bound registered at `at`, blamed on the argument whose type `is_of`
    /// holds of, when exactly one does.
    pub(super) fn of<'db>(
        cx: &Cx<'_, 'db>,
        at: InFile<ast::Node<'db>>,
        is_of: impl Fn(&hir::Type<'db>) -> bool,
    ) -> Cause {
        let plain = || Cause {
            span: cx.span(at.map(|it| it.text_range())),
            callee_span: None,
        };
        let (callee_range, args): (TextRange, Option<ast::ArgList<'db>>) =
            if let Some(path) = ast::PathExpr::cast(at.value)
                && let Some(call) = path.syntax().parent().and_then(ast::CallExpr::cast)
                && call.expr().is_some_and(|it| it.syntax() == path.syntax())
            {
                (path.syntax().text_range(), call.arg_list())
            } else if let Some(call) = ast::MethodCallExpr::cast(at.value) {
                let Some(name) = call.name_ref() else { return plain() };
                (name.syntax().text_range(), call.arg_list())
            } else {
                return plain();
            };
        let callee_span = cx.span(at.with_value(callee_range));
        let mut blamed = args
            .into_iter()
            .flat_map(|it| it.args())
            .filter(|arg| cx.vsema.type_of_expr(*arg).is_some_and(|it| is_of(&it.original)));
        match (blamed.next(), blamed.next()) {
            (Some(arg), None) => Cause {
                span: cx.span(at.with_value(arg.syntax().text_range())),
                callee_span: Some(callee_span),
            },
            _ => Cause { span: callee_span, callee_span: None },
        }
    }

    /// rustc's "required by a bound introduced by this call" on the callee, when the error is
    /// on one of its arguments.
    pub(super) fn label_call(&self, diag: &mut Diag) {
        if let Some(callee) = &self.callee_span
            && callee != &self.span
        {
            // clone: on the error path, for an error hir found
            diag.span_label(callee.clone(), "required by a bound introduced by this call");
        }
    }
}

/// rustc's `ObligationCauseCode::ImplDerived` notes: for each impl that needed the bound, from
/// the innermost out, "required for `X` to implement `Y`" (on a local impl's trait and self
/// type, "unsatisfied trait bound introduced here" on its bound that did not hold), the steps
/// of a trait already named counted as "N redundant requirements hidden".
fn note_derived<'db>(cx: &Cx<'_, 'db>, d: &hir::UnimplementedTrait<'db>, diag: &mut Diag) {
    let parents = &d.parent_trait_predicates;
    let required = |pred: &hir::TraitClause<'db>| {
        let tr = pred.trait_ref();
        let self_ty = tr.get_type_argument(0).map(|it| cx.ty(&it)).unwrap_or_default();
        // alloc: the error's text, for an error hir found
        format!("required for `{self_ty}` to implement `{}`", print::trait_path_sugared(cx, pred))
    };
    // alloc: the error's parts, for an error hir found
    let mut seen: Vec<hir::Trait> = Vec::new();
    let mut i = 0;
    while i < parents.len() {
        let trait_ = parents[i].trait_ref().trait_();
        let bound = d.parent_impl_bounds.get(i).copied().flatten();
        match d.parent_impls.get(i).copied().flatten().and_then(|it| impl_note(cx, it, bound)) {
            Some(at) => diag.span_note(at, required(&parents[i])),
            None => diag.note(required(&parents[i])),
        };
        if !seen.contains(&trait_) {
            seen.push(trait_);
        }
        let mut j = i + 1;
        while j < parents.len() && seen.contains(&parents[j].trait_ref().trait_()) {
            j += 1;
        }
        let count = j - i - 1;
        if count > 0 {
            let s = if count == 1 { "" } else { "s" };
            // alloc: the error's text, for an error hir found
            diag.note(format!("{count} redundant requirement{s} hidden"));
            diag.note(required(&parents[j - 1]));
        }
        if let Some(next) = parents.get(j) {
            seen.push(next.trait_ref().trait_());
        }
        i = j;
    }
}

/// Where rustc puts "required for `X` to implement `Y`" for a local impl: on its trait and self
/// type, "unsatisfied trait bound introduced here" on the impl's `bound` the step went through
/// ("implicitly introduced here" on the type parameter for an implicit `Sized`). `None` for an
/// impl of another crate or of a macro's expansion, which rustc notes without a span.
fn impl_note(cx: &Cx<'_, '_>, impl_: hir::Impl, bound: Option<hir::BoundSource>) -> Option<MultiSpan> {
    if impl_.module(cx.db).krate(cx.db) != cx.krate {
        return None;
    }
    let src = cx.vsema.source(impl_)?;
    if src.file_id.is_macro() {
        return None;
    }
    let item = src.value;
    let self_ty = item.self_ty()?;
    let self_span = cx.span(src.with_value(self_ty.syntax().text_range()));
    // alloc: the error's parts, for an error hir found
    let mut primary = Vec::with_capacity(2);
    if let Some(tr) = item.trait_() {
        primary.push(cx.span(src.with_value(tr.syntax().text_range())));
    }
    // clone: on the error path, for an error hir found
    primary.push(self_span.clone());
    // alloc: empty, the error's labels, for an error hir found
    let mut at = MultiSpan { primary, labels: Vec::new() };
    let label = match bound {
        Some(hir::BoundSource::ImplicitSized(_)) => "unsatisfied trait bound implicitly introduced here",
        _ => "unsatisfied trait bound introduced here",
    };
    if let Some(span) = bound.and_then(|it| obligation_cause::bound_span(cx, it))
        && !overlaps(&span, &self_span)
    {
        // alloc: the error's text, for an error hir found
        at.labels.push((span, label.to_owned()));
    }
    Some(at)
}

/// The `Assoc = ..` constraint on `assoc` written in `bound`'s trait path.
pub(super) fn constraint_range(cx: &Cx<'_, '_>, bound: ast::TypeBound<'_>, assoc: hir::TypeAlias) -> Option<TextRange> {
    let ast::Type::PathType(ty) = bound.ty()? else { return None };
    let name = assoc.name(cx.db);
    ty.path()?.segment()?.generic_arg_list()?.generic_args().find_map(|arg| match arg {
        ast::GenericArg::AssocTypeArg(it) if it.name_ref().is_some_and(|n| n.text() == name.as_str()) => {
            Some(it.syntax().text_range())
        }
        _ => None,
    })
}

pub(super) fn overlaps(a: &Span, b: &Span) -> bool {
    a.file == b.file && a.lo < b.hi && b.lo < a.hi
}

/// Whether the end of `a` is on the line `b` starts on (rustc compares `ident.span.hi()` with
/// `span.lo()`).
pub(super) fn same_line(cx: &Cx<'_, '_>, a: &Span, b: &Span) -> bool {
    if a.file != b.file {
        return false;
    }
    let Some(file) = cx.file_of(a) else { return true };
    let text = cx.file_text(file);
    let (lo, hi) = (a.hi.min(b.lo) as usize, a.hi.max(b.lo) as usize);
    text.get(lo..hi).is_none_or(|between| !between.contains('\n'))
}

/// What the trait's `#[rustc_on_unimplemented]` or `#[diagnostic::on_unimplemented]` says of
/// the predicate: rustc's `OnUnimplementedNote`.
#[derive(Default)]
pub(super) struct Custom {
    pub(super) message: Option<String>,
    pub(super) label: Option<String>,
    pub(super) notes: Vec<String>,
}

/// A part of the attribute: `word`, `name = "value"` or `name(..)`.
#[derive(Debug, Clone, PartialEq)]
enum Meta {
    Word(String),
    Value(String, String),
    List(String, Vec<Meta>),
}

/// rustc's `OnUnimplementedDirective::evaluate` of the attributes of the trait of `pred`.
fn on_unimplemented<'db>(cx: &Cx<'_, 'db>, pred: &hir::TraitClause<'db>, direct: bool) -> Custom {
    let tr = pred.trait_ref();
    let trait_ = tr.trait_();
    let Some(src) = cx.vsema.source(trait_) else { return Custom::default() };
    let mut args = None;
    let mut custom = Custom::default();
    for attr in src.value.attrs() {
        let Some(ast::Meta::TokenTreeMeta(meta)) = attr.meta() else { continue };
        let Some(path) = meta.path() else { continue };
        let path = path.syntax().text();
        let is_rustc = path == "rustc_on_unimplemented";
        if !is_rustc && path != "diagnostic::on_unimplemented" {
            continue;
        }
        let Some(tt) = meta.token_tree() else { continue };
        let args: &Args = args.get_or_insert_with(|| Args::of(cx, pred));
        let metas = parse_metas(tt.syntax());
        // rustc walks the directive, then its `on(..)`s from the last to the first, each that
        // holds overriding the message and label before it and adding its notes: the first
        // `on` that holds wins, the directive's own fields are the default.
        // alloc: the error's parts, for an error hir found
        let mut commands: Vec<(Option<&Meta>, Vec<&Meta>)> = vec![(None, Vec::new())];
        for meta in &metas {
            match meta {
                Meta::List(name, inner) if name == "on" && is_rustc => {
                    // alloc: the error's parts, for an error hir found
                    let (cond, fields) = inner.split_first().map_or((None, Vec::new()), |(c, f)| {
                        // alloc: the error's parts, for an error hir found
                        (Some(c), f.iter().collect())
                    });
                    commands.push((cond, fields));
                }
                _ => commands[0].1.push(meta),
            }
        }
        let mut found = Custom::default();
        for (cond, fields) in std::iter::once(&commands[0]).chain(commands[1..].iter().rev()) {
            if let Some(cond) = cond
                && !args.holds(cond, direct)
            {
                continue;
            }
            for field in fields {
                let Meta::Value(name, value) = field else { continue };
                let text = args.format(value);
                match name.as_str() {
                    "message" => found.message = Some(text),
                    "label" => found.label = Some(text),
                    "note" => found.notes.push(text),
                    _ => {}
                }
            }
        }
        custom.message = custom.message.or(found.message);
        custom.label = custom.label.or(found.label);
        custom.notes.extend(found.notes);
    }
    custom
}

/// The parts of an attribute's token tree, `(a, b = "c", d(e))`.
fn parse_metas(tt: ast::Node<'_>) -> Vec<Meta> {
    // alloc: the error's parts, for an error hir found
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    let mut eq = false;
    let mut items = tt.children_with_tokens().peekable();
    // The opening and closing delimiters.
    items.next();
    for item in items {
        match item {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::TOKEN_TREE => {
                if let Some(name) = name.take() {
                    out.push(Meta::List(name, parse_metas(node)));
                }
                eq = false;
            }
            NodeOrToken::Node(_) => {}
            NodeOrToken::Token(token) => match token.kind() {
                kind if kind.is_trivia() => {}
                SyntaxKind::COMMA | SyntaxKind::R_PAREN | SyntaxKind::R_BRACK | SyntaxKind::R_CURLY => {
                    if let Some(name) = name.take() {
                        out.push(Meta::Word(name));
                    }
                    eq = false;
                }
                SyntaxKind::EQ => eq = true,
                SyntaxKind::STRING => {
                    let value = ast::String::cast(token)
                        .and_then(|it| it.value().ok().map(|it| it.into_owned()))
                        .unwrap_or_default();
                    if let (Some(n), true) = (name.take(), eq) {
                        out.push(Meta::Value(n, value));
                    }
                    eq = false;
                }
                _ => {
                    // alloc: the error's text, for an error hir found
                    let text = token.text().to_owned();
                    name = Some(match name.take() {
                        Some(prefix) => prefix + &text,
                        None => text,
                    });
                }
            },
        }
    }
    if let Some(name) = name {
        out.push(Meta::Word(name));
    }
    out
}

/// What the format strings and conditions of `on_unimplemented` are of.
struct Args {
    /// The self type as diagnostics print it, for `{Self}`.
    self_ty: String,
    /// What `Self = ".."` is compared with: the type with its whole path, its type's generic
    /// form, and its kind (`{union}`).
    self_matches: Vec<String>,
    /// The trait's name, for `{This}`.
    this: String,
    trait_path: String,
    /// The trait's generic parameters and what they are, for `{T}` and `T = ".."`.
    generics: Vec<(String, String)>,
    /// Whether the self type is an ADT of the crate being checked (`crate_local`).
    crate_local: bool,
}

impl Args {
    fn of<'db>(cx: &Cx<'_, 'db>, pred: &hir::TraitClause<'db>) -> Args {
        let tr = pred.trait_ref();
        let trait_ = tr.trait_();
        let self_ty = tr.get_type_argument(0);
        // alloc: the error's parts, for an error hir found
        let mut self_matches = Vec::new();
        let mut crate_local = false;
        if let Some(ty) = &self_ty {
            self_matches.push(cx.ty(ty));
            if let Some(adt) = ty.as_adt() {
                crate_local = is_local_adt(cx, adt);
                if let Some(path) = full_path(cx, adt) {
                    // alloc: the error's parts, for an error hir found
                    let args: Vec<String> = ty.type_arguments().map(|it| cx.ty(&it)).collect();
                    let params: Vec<String> = hir::GenericDef::Adt(adt)
                        .type_or_const_params(cx.db)
                        .into_iter()
                        // alloc: the error's text, for an error hir found
                        .map(|it| it.name(cx.db).as_str().to_owned())
                        // alloc: the error's parts, for an error hir found
                        .collect();
                    if args.is_empty() {
                        // clone: on the error path, for an error hir found
                        self_matches.push(path.clone());
                    } else {
                        // alloc: the error's text, for an error hir found
                        self_matches.push(format!("{path}<{}>", args.join(", ")));
                        // alloc: the error's text, for an error hir found
                        self_matches.push(format!("{path}<{}>", params.join(", ")));
                    }
                }
                self_matches.push(
                    match adt {
                        hir::Adt::Struct(_) => "{struct}",
                        hir::Adt::Enum(_) => "{enum}",
                        hir::Adt::Union(_) => "{union}",
                    }
                    // alloc: the error's text, for an error hir found
                    .to_owned(),
                );
            } else if ty.is_int_or_uint() {
                // alloc: the error's text, for an error hir found
                self_matches.push("{integral}".to_owned());
            } else if ty.is_float() {
                // alloc: the error's text, for an error hir found
                self_matches.push("{float}".to_owned());
            }
        }
        // alloc: the error's parts, for an error hir found
        let mut generics = Vec::new();
        let lifetimes = hir::GenericDef::Trait(trait_).lifetime_params(cx.db).len();
        for (i, param) in hir::GenericDef::Trait(trait_).type_or_const_params(cx.db).into_iter().enumerate() {
            // alloc: the error's text, for an error hir found
            let name = param.name(cx.db).as_str().to_owned();
            if name == "Self" {
                continue;
            }
            // The arguments are the self type, then the trait's own, lifetimes last.
            if lifetimes == 0
                && let Some(arg) = tr.get_type_argument(i)
            {
                generics.push((name, cx.ty(&arg)));
            }
        }
        Args {
            self_ty: self_ty.as_ref().map(|it| cx.ty(it)).unwrap_or_default(),
            self_matches,
            // alloc: the error's text, for an error hir found
            this: trait_.name(cx.db).as_str().to_owned(),
            trait_path: print::trait_path(cx, &tr),
            generics,
            crate_local,
        }
    }

    /// rustc's `OnUnimplementedCondition::matches_predicate`.
    fn holds(&self, cond: &Meta, direct: bool) -> bool {
        match cond {
            Meta::Word(word) => match word.as_str() {
                "crate_local" => self.crate_local,
                "direct" => direct,
                _ => false,
            },
            Meta::Value(name, value) => match name.as_str() {
                "Self" | "_Self" => {
                    let value = self.format(value);
                    self.self_matches.iter().any(|it| *it == value)
                }
                _ => self
                    .generics
                    .iter()
                    .any(|(param, arg)| param == name && *arg == self.format(value)),
            },
            Meta::List(name, inner) => match name.as_str() {
                "not" => inner.first().is_some_and(|it| !self.holds(it, direct)),
                "all" => inner.iter().all(|it| self.holds(it, direct)),
                "any" => inner.iter().any(|it| self.holds(it, direct)),
                _ => false,
            },
        }
    }

    /// rustc's `FormatString::format`: `{Self}`, `{This}`, `{Trait}` and the trait's generic
    /// names filled in, `{{` and `}}` as braces; the kinds `Self = ".."` compares with
    /// (`{union}`) are left as they are.
    fn format(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(['{', '}']) {
            out.push_str(&rest[..at]);
            let tail = &rest[at..];
            if tail.starts_with("{{") || tail.starts_with("}}") {
                out.push_str(&tail[..1]);
                rest = &tail[2..];
                continue;
            }
            if tail.starts_with('}') {
                out.push('}');
                rest = &tail[1..];
                continue;
            }
            let Some(end) = tail.find('}') else {
                out.push_str(tail);
                rest = "";
                break;
            };
            let name = &tail[1..end];
            match name {
                "Self" => out.push_str(&self.self_ty),
                "This" => out.push_str(&self.this),
                "Trait" => out.push_str(&self.trait_path),
                _ => match self.generics.iter().find(|(param, _)| param == name) {
                    Some((_, arg)) => out.push_str(arg),
                    None => out.push_str(&tail[..=end]),
                },
            }
            rest = &tail[end + 1..];
        }
        out.push_str(rest);
        out
    }
}

/// An ADT's path with its crate's name, as rustc prints it with no trimmed paths.
fn full_path(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<String> {
    let krate = adt.module(cx.db).krate(cx.db).display_name(cx.db)?;
    let path = hir::ModuleDef::Adt(adt).canonical_path(cx.db, cx.edition)?;
    // alloc: the error's text, for an error hir found
    Some(format!("{krate}::{path}"))
}

/// The name rustc's `get_diagnostic_name` gives a trait it can derive: core's lang and
/// diagnostic items, or a trait marked `#[rustc_diagnostic_item = ".."]`.
fn derivable_name(cx: &Cx<'_, '_>, trait_: hir::Trait) -> Option<&'static str> {
    const LANG: [(LangItem, &str); 4] = [
        (LangItem::Copy, "Copy"),
        (LangItem::Clone, "Clone"),
        (LangItem::PartialEq, "PartialEq"),
        (LangItem::PartialOrd, "PartialOrd"),
    ];
    const DERIVABLE: [&str; 9] =
        ["Copy", "Clone", "PartialEq", "PartialOrd", "Eq", "Ord", "Hash", "Debug", "Default"];
    if let Some((_, name)) = LANG.iter().find(|(item, _)| is_lang(cx, trait_, *item)) {
        return Some(name);
    }
    if trait_.module(cx.db).krate(cx.db).origin(cx.db) == CrateOrigin::Lang(LangCrateOrigin::Core)
        && let Some(name) = DERIVABLE[4..].iter().find(|it| trait_.name(cx.db).as_str() == **it)
    {
        return Some(name);
    }
    let src = cx.vsema.source(trait_)?;
    let named = src.value.attrs().find_map(|attr| match attr.meta()? {
        ast::Meta::KeyValueMeta(meta)
            if meta.path()?.syntax().text() == "rustc_diagnostic_item" =>
        {
            let ast::Expr::Literal(lit) = meta.expr()? else { return None };
            let ast::LiteralKind::String(s) = lit.kind() else { return None };
            s.value().ok()
        }
        _ => None,
    })?;
    DERIVABLE.into_iter().find(|name| *name == named)
}

/// rustc's `can_suggest_derive`: the self type is a local ADT, the trait one it can derive,
/// and every field implements it.
fn can_suggest_derive(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>) -> bool {
    if pred.polarity() == hir::ClausePolarity::Negative {
        return false;
    }
    let tr = pred.trait_ref();
    let trait_ = tr.trait_();
    let Some(name) = derivable_name(cx, trait_) else { return false };
    let Some(self_ty) = tr.get_type_argument(0) else { return false };
    let Some(adt) = self_ty.as_adt() else { return false };
    if !is_local_adt(cx, adt) {
        return false;
    }
    let derivable = match name {
        "Copy" | "Clone" => true,
        _ if matches!(adt, hir::Adt::Union(_)) => false,
        "PartialEq" | "PartialOrd" => {
            tr.get_type_argument(1).is_some_and(|rhs| rhs == self_ty)
        }
        _ => true,
    };
    if !derivable {
        return false;
    }
    // alloc: the error's parts, for an error hir found
    let args: Vec<hir::Type<'_>> = self_ty.type_arguments().collect();
    let has_rhs = matches!(name, "PartialEq" | "PartialOrd");
    let holds = |field: hir::Field| {
        let ty = field.ty(cx.db).instantiate_type_params(cx.db, args.iter());
        let rhs: &[hir::Type<'_>] = if has_rhs { std::slice::from_ref(&ty) } else { &[] };
        ty.impls_trait(cx.db, trait_, rhs)
    };
    match adt {
        hir::Adt::Struct(it) => it.fields_iter(cx.db).all(holds),
        hir::Adt::Union(it) => it.fields_iter(cx.db).all(holds),
        hir::Adt::Enum(it) => it.variants_iter(cx.db).all(|v| v.fields_iter(cx.db).all(holds)),
    }
}

/// rustc's `suggest_derive`: "consider annotating `S` with `#[derive(Trait)]`".
fn suggest_derive<'db>(cx: &Cx<'_, 'db>, pred: &hir::TraitClause<'db>, diag: &mut Diag) {
    let tr = pred.trait_ref();
    let Some(name) = derivable_name(cx, tr.trait_()) else { return };
    let Some(self_ty) = tr.get_type_argument(0) else { return };
    let Some(adt) = self_ty.as_adt() else { return };
    let Some(at) = adt_def_span(cx, adt) else { return };
    diag.suggestion_verbose(
        at.shrink_to_lo(),
        // alloc: the error's text, for an error hir found
        format!("consider annotating `{}` with `#[derive({name})]`", cx.ty(&self_ty)),
        // alloc: the error's text, for an error hir found
        format!("#[derive({name})]\n"),
        Applicability::MaybeIncorrect,
    );
}

/// The span rustc's `def_span` gives an ADT: its header.
fn adt_def_span(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<Span> {
    let src = cx.vsema.source(adt)?;
    Some(cx.span(src.map(|it| def_range(it.syntax()))))
}

fn is_local_adt(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    adt.module(cx.db).krate(cx.db) == cx.krate
}

fn is_local_trait(cx: &Cx<'_, '_>, trait_: hir::Trait) -> bool {
    trait_.module(cx.db).krate(cx.db) == cx.krate
}

/// Whether the crate being checked has an impl of `trait_`, one of its own traits: rustc's
/// `all_impls(..)` of the local crate, which is all a local trait can have (crates after it are
/// not seen). Reads the trait's row of the crate's impl table and of the trait's block's, both
/// memoized queries, and allocates nothing.
fn has_impls(cx: &Cx<'_, '_>, trait_: hir::Trait) -> bool {
    let id = hir_def::TraitId::from(trait_);
    let module = hir_def::ModuleId::from(trait_.module(cx.db));
    sees(TraitImpls::for_crate(cx.db, cx.krate.into()), id)
        || module.block(cx.db).and_then(|it| TraitImpls::for_block(cx.db, it)).is_some_and(|it| sees(it, id))
}

fn sees(impls: &TraitImpls<'_>, trait_: hir_def::TraitId) -> bool {
    let mut any = false;
    impls.for_trait(trait_, |run| any |= run.either(|it| !it.is_empty(), |it| !it.is_empty()));
    any
}

pub(super) fn is_lang(cx: &Cx<'_, '_>, trait_: hir::Trait, item: LangItem) -> bool {
    hir::Trait::lang(cx.db, cx.krate, item) == Some(trait_)
}

/// Whether the trait is `#[unstable]`: rustc then calls it "nightly-only, unstable".
fn is_unstable(cx: &Cx<'_, '_>, trait_: hir::Trait) -> bool {
    let Some(src) = cx.vsema.source(trait_) else { return false };
    src.value.attrs().any(|attr| attr.path().is_some_and(|it| it.syntax().text() == "unstable"))
}
