//! `error_debug`: an error's message that formats a value with `{:?}`, or formats a value whose
//! formatting needs the attached database outside `hir_ty::attach_db` (AGENTS.md rule 15:
//! formatting an error must not panic; the 2026-11-01 macro-eval panic).
//!
//! An error's message is one of:
//!
//! - the body of `Display::fmt` of a type that implements `core::error::Error`;
//! - a `format_args!` (through `format!`, `write!` and any macro that expands to one) whose value
//!   flows, through calls, blocks and conversions, into an expression whose type implements
//!   `Error`: `MyError::Parse(format!(..))`, `Error::new(format!(..))`;
//! - the template of a `#[error("..")]` attribute on an error type or its variant (thiserror's
//!   derive writes the `Display` impl, whose body blaze does not walk).
//!
//! In one, a placeholder `{:?}` is a finding, whatever it formats: an error says what it is about
//! in words. A placeholder of any trait whose value needs the attached database is a finding
//! unless the `format_args!` is inside a closure handed to `hir_ty::attach_db`,
//! `attach_db_allow_change` or `with_attached_db`. A value needs it when its type is an ADT of
//! `hir_ty` (its `next_solver` types included), or has one as a generic argument, or is an ADT
//! of outside the standard library a field of which (three ADTs deep) does.
//!
//! What is formatted is read off the lowered `format_args!`: each argument is a call of
//! `core::fmt::rt::Argument::new_debug` (or `new_display`, `new_lower_hex`, ...), found by the
//! lang item `format_argument` and the function's name, with the argument's type as its generic
//! argument. So a placeholder captured from the scope (`{ty:?}`), a renamed import of the macro
//! and a `format_args!` written by a macro are all the same.

use std::cell::OnceCell;

use base_db::Db;
use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId, HasModule as _, ItemContainerId,
    expr_store::Body,
    hir::{Array, Expr, ExprId, Statement},
    lang_item::lang_items,
    signatures::FunctionSignature,
};
use hir_ty::InferenceResult;
use intern::sym;
use syntax::{
    NodeOrToken, SyntaxKind,
    view::{self as ast, AstNode as _, AstToken as _, HasAttrs as _, HasName as _},
};

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts::{self, Parents},
};
use crate::{
    check::Cx,
    lints::format::{Arg, Placeholders, Trait},
};

/// How many expressions up a `format_args!` is followed to the error it is the message of.
const MESSAGE_DEPTH: usize = 12;
/// How many expressions up a `format_args!` is looked at for the closure of an attach.
const ATTACH_DEPTH: usize = 64;
/// How many ADTs deep a type is looked into for one of `hir_ty`'s.
const FIELD_DEPTH: u32 = 3;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(Lint::ErrorDebug) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// The traits an error's message is judged by.
struct Traits {
    error: hir::Trait,
    display: hir::Trait,
}

fn traits(cx: &Cx<'_, '_>) -> Option<Traits> {
    Some(Traits { error: cx.std_trait(&["core", "error", "Error"])?, display: cx.std_trait(&["core", "fmt", "Display"])? })
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let Some(traits) = traits(cx) else { return found };
    let db = cx.db();
    let Some(argument) = lang_items(db, def.krate(db)).FormatArgument else { return found };
    let body = analysis.body();
    let infer = analysis.infer();
    let display_of = error_display(db, def, &traits);
    let parents_of = OnceCell::new();
    let parents = || parents_of.get_or_init(|| Parents::of(body));
    let target = cx.krate().to_display_target(db);
    for (id, expr) in body.exprs() {
        let Expr::Block { statements, .. } = expr else { continue };
        let Some(formatted) = formatted(db, body, infer, argument, statements) else { continue };
        let error_name = match &display_of {
            Some(error) => error.display(db, target).to_string(),
            None => match message_of(db, analysis, parents(), id, &traits) {
                Some(error) => error.display(db, target).to_string(),
                None => continue,
            },
        };
        let mut attached = None;
        for Formatted { value, debug, ty } in formatted {
            // A name captured from the template has no syntax of its own: the finding is at
            // the `format_args!`, or the macro call that wrote it.
            let range = value.and_then(|it| analysis.expr_range(it)).or_else(|| analysis.expr_range(id));
            let Some(range) = range else { continue };
            let ty = analysis.ty(ty);
            let shown = ty.display(db, target).to_string();
            if debug {
                let message = format!("the message of error `{error_name}` formats `{shown}` with `{{:?}}`");
                let note = "an error says what it is about in words: name the item, the key or the file \
                            with `Display`, which needs nothing the error outlives";
                facts::push(&mut found, Lint::ErrorDebug, range, message, Some(note.to_owned()));
                continue;
            }
            let Some(held) = needs_db(db, &ty) else { continue };
            if *attached.get_or_insert_with(|| is_attached(db, body, infer, parents(), id)) {
                continue;
            }
            let message = format!(
                "the message of error `{error_name}` formats `{shown}`, which needs the attached database, outside `hir_ty::attach_db`"
            );
            let panics = "formatting it outside `hir_ty::attach_db` panics: render what needs the database while it is attached";
            let note = match ty.strip_references().as_adt() == Some(held) {
                true => panics.to_owned(),
                false => format!("it holds `hir_ty`'s `{}`; {panics}", held.name(db).as_str()),
            };
            facts::push(&mut found, Lint::ErrorDebug, range, message, Some(note));
        }
    }
    found
}

/// What one placeholder of a `format_args!` formats.
struct Formatted<'db> {
    /// The argument, its `&` taken away; one captured from the template by name is a path the
    /// lowering made, with no range of its own.
    value: Option<ExprId>,
    /// Whether it formats with `Debug`.
    debug: bool,
    ty: hir_ty::next_solver::Ty<'db>,
}

/// What the block with `statements` formats, if it is a lowered `format_args!` with arguments:
/// `let args = (&a, &b); let args = [Argument::new_debug(args.0), ..];`.
fn formatted<'db>(
    db: &Db,
    body: &Body,
    infer: &InferenceResult<'db>,
    argument: hir_def::StructId,
    statements: hir_def::hir::Statements<'_>,
) -> Option<Vec<Formatted<'db>>> {
    let mut lets = statements.iter().filter_map(|it| match it {
        Statement::Let { initializer: Some(it), .. } => Some(it),
        _ => None,
    });
    let (Some(tuple), Some(array)) = (lets.next(), lets.next()) else { return None };
    let Expr::Tuple { exprs: written } = body.expr(tuple) else { return None };
    let Expr::Array(Array::ElementList { elements }) = body.expr(array) else { return None };
    // alloc: the arguments of one `format_args!`, a handful
    let mut formatted = Vec::with_capacity(elements.len());
    for &element in elements {
        let Expr::Call { args: [arg], .. } = body.expr(element) else { return None };
        let (f, args) = written_callee(infer, body, element)?;
        if !is_argument_new(db, f, argument) {
            return None;
        }
        let name = &FunctionSignature::of(db, f).name;
        if *name == sym::from_usize {
            continue;
        }
        let Expr::Field { name: field, .. } = body.expr(*arg) else { return None };
        let value = field.as_tuple_index().and_then(|it| written.get(it)).map(|&it| match body.expr(it) {
            Expr::Ref { expr, .. } => expr,
            _ => it,
        });
        let ty = args.types().next()?;
        formatted.push(Formatted { value, debug: *name == sym::new_debug, ty });
    }
    Some(formatted)
}

/// Whether `f` is a function of `core::fmt::rt::Argument`'s impl.
fn is_argument_new(db: &Db, f: hir_def::FunctionId, argument: hir_def::StructId) -> bool {
    let ItemContainerId::ImplId(it) = f.loc(db).container else { return false };
    hir::Impl::from(it).self_ty(db).as_adt() == Some(hir::Adt::Struct(argument.into()))
}

/// The error type `def` is the `Display::fmt` of, if it is one.
fn error_display<'db>(db: &'db Db, def: DefWithBodyId, traits: &Traits) -> Option<hir::Type<'db>> {
    let DefWithBodyId::FunctionId(f) = def else { return None };
    let ItemContainerId::ImplId(it) = f.loc(db).container else { return None };
    let imp = hir::Impl::from(it);
    if imp.trait_(db)? != traits.display {
        return None;
    }
    let self_ty = imp.self_ty(db);
    self_ty.impls_trait(db, traits.error, &[]).then_some(self_ty)
}

/// The error the `format_args!` `id` is the message of: the first expression up from it whose
/// type implements `Error`, before a closure or a loop ends the value's way. Blocks are gone
/// through, statements included: `format!` binds its `String` in a `let` of its own block.
fn message_of<'db>(
    db: &'db Db,
    analysis: &hir::BodyAnalysis<'db>,
    parents: &Parents,
    id: ExprId,
    traits: &Traits,
) -> Option<hir::Type<'db>> {
    let body = analysis.body();
    let infer = analysis.infer();
    for up in parents.up(id, MESSAGE_DEPTH) {
        if matches!(body.expr(up), Expr::Closure { .. } | Expr::Loop { .. }) {
            return None;
        }
        let ty = analysis.ty(infer.expr_ty(up));
        if ty.impls_trait(db, traits.error, &[]) {
            return Some(ty);
        }
    }
    None
}

/// Whether the `format_args!` `id` is inside a closure handed to `hir_ty::attach_db`,
/// `attach_db_allow_change` or `with_attached_db`.
fn is_attached(db: &Db, body: &Body, infer: &InferenceResult<'_>, parents: &Parents, id: ExprId) -> bool {
    let mut closure = None;
    for up in parents.up(id, ATTACH_DEPTH) {
        if let Some(inner) = closure.take()
            && let Expr::Call { args, .. } = body.expr(up)
            && args.contains(&inner)
            && let Some((f, _)) = written_callee(infer, body, up)
            && is_attach(db, f)
        {
            return true;
        }
        if matches!(body.expr(up), Expr::Closure { .. }) {
            closure = Some(up);
        }
    }
    false
}

fn is_attach(db: &Db, f: hir_def::FunctionId) -> bool {
    let name = &FunctionSignature::of(db, f).name;
    matches!(name.as_str(), "attach_db" | "attach_db_allow_change" | "with_attached_db")
        && facts::crate_is(db, hir::Crate::from(f.krate(db)), "hir_ty")
}

/// The ADT of `hir_ty` that formatting `ty` reads the attached database for, if one is: `ty`'s
/// own, a generic argument's, or a field's of an ADT outside the standard library.
pub(super) fn needs_db(db: &Db, ty: &hir::Type<'_>) -> Option<hir::Adt> {
    // alloc: the ADTs looked into for one formatted value, a few
    let mut seen = Vec::new();
    needs_db_in(db, ty, FIELD_DEPTH, &mut seen)
}

fn needs_db_in(db: &Db, ty: &hir::Type<'_>, depth: u32, seen: &mut Vec<hir::Adt>) -> Option<hir::Adt> {
    // alloc: the ADTs of one type's walk, a few
    let mut adts = Vec::new();
    ty.walk(db, |it| adts.extend(it.as_adt()));
    for adt in adts {
        if seen.contains(&adt) {
            continue;
        }
        seen.push(adt);
        let krate = adt.module(db).krate(db);
        if facts::crate_is(db, krate, "hir_ty") {
            return Some(adt);
        }
        if depth == 0 || krate.origin(db).is_lang() {
            continue;
        }
        for field in fields(db, adt) {
            if let Some(found) = needs_db_in(db, &field.ty(db), depth - 1, seen) {
                return Some(found);
            }
        }
    }
    None
}

/// Every field of `adt`, of every variant.
fn fields(db: &Db, adt: hir::Adt) -> Vec<hir::Field> {
    match adt {
        hir::Adt::Struct(it) => it.fields(db),
        hir::Adt::Union(it) => it.fields(db),
        // alloc: an enum's fields, once per ADT looked into
        hir::Adt::Enum(it) => it.variants(db).into_iter().flat_map(|it| it.fields(db)).collect(),
    }
}

/// The findings of `#[error("..")]` templates on `adt`, a type implementing `Error`, and on its
/// variants: thiserror's derive writes the `Display` impl from them.
pub(super) fn check_attrs<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(Lint::ErrorDebug) {
        return;
    }
    let Some(traits) = traits(cx) else { return };
    let db = cx.db();
    let Some(def) = cx.adt_def(adt) else { return };
    let ty = def.ty(db);
    if !ty.impls_trait(db, traits.error, &[]) {
        return;
    }
    let error_name = def.name(db).as_str().to_owned();
    match (adt, def) {
        (ast::Adt::Enum(written), hir::Adt::Enum(it)) => {
            let variants = it.variants(db);
            for variant in written.variant_list().into_iter().flat_map(|it| it.variants()) {
                let Some(name) = variant.name() else { continue };
                let Some(hir_variant) = variants.iter().find(|it| it.name(db).as_str() == name.text()) else { continue };
                let fields = hir_variant.fields(db);
                let named = format!("{error_name}::{}", name.text());
                check_owner(cx, variant.attrs(), &fields, &named);
            }
            check_owner(cx, adt.attrs(), &[], &error_name);
        }
        (_, hir::Adt::Struct(it)) => check_owner(cx, adt.attrs(), &it.fields(db), &error_name),
        _ => {}
    }
}

/// The findings of the `#[error("..")]` templates among `attrs`, of an error whose fields are
/// `fields`, named `error_name` in the messages.
fn check_owner<'t>(
    cx: &mut Cx<'_, 't>,
    attrs: impl Iterator<Item = ast::Attr<'t>>,
    fields: &[hir::Field],
    error_name: &str,
) {
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    for attr in attrs {
        let Some(ast::Meta::TokenTreeMeta(meta)) = attr.meta() else { continue };
        if meta.path().and_then(|it| it.as_single_name_ref()).is_none_or(|it| it.text() != "error") {
            continue;
        }
        let Some(tt) = meta.token_tree() else { continue };
        let Some(template) = tt.syntax().children_with_tokens().find_map(|it| match it {
            NodeOrToken::Token(it) if it.kind() == SyntaxKind::STRING => ast::String::cast(it),
            _ => None,
        }) else {
            continue;
        };
        let Ok(text) = template.value() else { continue };
        let Some(range) = cx.range(attr.syntax()) else { continue };
        // A template handed arguments of its own (`#[error("{}", .0.name)]`) formats those, not
        // fields, by index; only `{:?}` is judged of them.
        let extra = tt
            .syntax()
            .children_with_tokens()
            .any(|it| matches!(it, NodeOrToken::Token(it) if it.kind() == SyntaxKind::COMMA));
        let field_at = |arg: Arg<'_>| -> Option<&hir::Field> {
            match arg {
                Arg::Index(_) if extra => None,
                Arg::Index(index) => fields.iter().find(|it| it.name(db).as_tuple_index() == Some(index)),
                Arg::Captured(name) => fields.iter().find(|it| it.name(db).as_str() == name),
            }
        };
        for placeholder in Placeholders::new(&text, |_| None) {
            let field = field_at(placeholder.arg);
            if placeholder.format_trait == Trait::Debug {
                let what = field.map_or_else(|| "a value".to_owned(), |it| format!("field `{}`", it.name(db).as_str()));
                let message = format!("the message of error `{error_name}` formats {what} with `{{:?}}`");
                let span = cx.span(range);
                cx.blaze(Lint::ErrorDebug, attr.syntax(), span, message)
                    .note("an error says what it is about in words: name the item, the key or the file with `Display`, which needs nothing the error outlives")
                    .help(Lint::ErrorDebug.help())
                    .help(format!("for further information visit {}", Lint::ErrorDebug.docs_url()));
                continue;
            }
            let Some(field) = field else { continue };
            let ty = field.ty(db);
            if needs_db(db, &ty).is_none() {
                continue;
            }
            let message = format!(
                "the message of error `{error_name}` formats field `{}` (`{}`), which needs the attached database, outside `hir_ty::attach_db`",
                field.name(db).as_str(),
                ty.display(db, target)
            );
            let span = cx.span(range);
            cx.blaze(Lint::ErrorDebug, attr.syntax(), span, message)
                .note("formatting it outside `hir_ty::attach_db` panics: render what needs the database into the error when it is made")
                .help(Lint::ErrorDebug.help())
                .help(format!("for further information visit {}", Lint::ErrorDebug.docs_url()));
        }
    }
}
