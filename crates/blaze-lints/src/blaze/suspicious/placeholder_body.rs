//! `placeholder_body`: a function whose whole body is a constant success (`Ok(())`, `Ok` or
//! `Some` of a constant, `None`, `true`, `0`, `()` or `{}`, a zero-argument `new()` or
//! `default()`, or `Default::default()`) while it takes inputs it never reads, or while its doc
//! says it does something (writes, sends, saves, removes, ...). It is a stub that reports work
//! it never did: the caller goes on as if the file were written or the entry removed, the
//! "no fallback, failures are loud" rule broken in the quietest way.
//!
//! The inputs are the parameters, `self` among them unless its type has no fields to read. A
//! trait's own default bodies are left alone (a hook that does nothing by default is the
//! design), and so is a trait impl's method, but where the trait's default body is the same
//! constant: then the override says nothing the trait does not and the fix deletes it. Test
//! code is left alone. Read from the body's HIR, not its text.

use hir_def::{
    VariantId,
    hir::{Expr, ExprId, Literal},
};
use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasName as _};

use super::super::{
    Lint,
    std_mistakes::{Fix, report},
    test_code,
};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::PlaceholderBody;

/// Words a doc uses to promise an effect the body must have.
const EFFECTS: &[&str] = &[
    "writes", "sends", "saves", "removes", "deletes", "stores", "records", "persists", "flushes",
    "creates", "updates", "registers", "spawns", "kills", "starts", "stops", "installs", "uploads",
    "publishes", "commits", "syncs", "applies", "inserts",
];

pub(super) fn check_body(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(f) = ast::Fn::cast(cx.root()) else { return };
    if f.body().is_none() || test_code::is_test_code(cx, f.syntax()) {
        return;
    }
    let Some(analysis) = cx.analysis() else { return };
    let hir::DefWithBody::Function(func) = analysis.def() else { return };
    let db = cx.db();
    let Some(shape) = constant_body(cx, analysis) else { return };
    let assoc = hir::AsAssocItem::as_assoc_item(func, db);
    if let Some(assoc) = assoc {
        if matches!(assoc.container(db), hir::AssocItemContainer::Trait(_)) {
            return;
        }
        if let Some(trait_) = assoc.implemented_trait(db) {
            same_as_default(cx, f, func, trait_, &shape);
            return;
        }
    }
    let unread = inputs(cx, f, func);
    // A constructor's doc says it creates: making the value is the effect it promises.
    let constructs = matches!(shape.as_str(), "new()" | "default()" | "empty()");
    let promised = if constructs { None } else { promise(f) };
    if unread.is_empty() && promised.is_none() {
        return;
    }
    let Some(name) = f.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let why = match (unread.is_empty(), promised) {
        (_, Some(word)) => format!("while its doc says it {word}"),
        _ => format!("and reads none of its inputs ({})", unread.join(", ")),
    };
    let message = format!(
        "the whole body of `{}` is `{shape}`, a success that does nothing, {why}: every caller \
         goes on as if the work were done",
        name.text()
    );
    let note = "do the work, or return an error that names what is not built yet: a loud failure \
                beats a quiet success";
    report(cx, LINT, f.syntax(), range, message, note, None);
}

/// A trait impl's method whose body is `shape`, the same constant as the trait's default body:
/// reported with a fix that deletes the override.
fn same_as_default<'t>(
    cx: &mut Cx<'_, 't>,
    f: ast::Fn<'t>,
    func: hir::Function,
    trait_: hir::Trait,
    shape: &str,
) {
    let db = cx.db();
    let name = func.name(db);
    let Some(default) = trait_.items(db).into_iter().find_map(|it| match it {
        hir::AssocItem::Function(it) if it.name(db) == name => Some(it),
        _ => None,
    }) else {
        return;
    };
    if !default.has_body(db) {
        return;
    }
    let inner = hir::BodyAnalysis::new(db, hir::DefWithBody::from(default));
    if constant_body(cx, &inner).as_deref() != Some(shape) {
        return;
    }
    let (Some(range), Some(at)) = (cx.range(f.syntax()), f.name().and_then(|it| cx.range(it.syntax()))) else {
        return;
    };
    let message = format!(
        "this `{}` is `{shape}`, what `{}`'s default body already is: the override says nothing",
        name.as_str(),
        trait_.name(db).as_str()
    );
    let note = "delete the override and let the trait's default stand";
    let fix = Fix {
        message: "delete the override",
        // alloc: the fix's one part
        parts: vec![(range, String::new())],
        applicability: Applicability::MachineApplicable,
    };
    report(cx, LINT, f.syntax(), at, message, note, Some(fix));
}

/// The body of `analysis` as the constant success it is, written as the message says it, or
/// `None` when it computes anything.
fn constant_body(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>) -> Option<String> {
    let body = analysis.body();
    let root = body.root_expr();
    match body.expr(root) {
        Expr::Block { statements, tail, .. } => {
            if statements.iter().next().is_some() {
                return None;
            }
            match tail {
                None => Some("{}".to_owned()),
                Some(tail) => constant(cx, analysis, tail, 0),
            }
        }
        _ => constant(cx, analysis, root, 0),
    }
}

/// `expr` as the constant success it is: a literal `true` or zero, `()`, `None`, `Ok` or `Some`
/// of one, or a call with no arguments of a function named `new`, `default` or `empty`.
fn constant(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, expr: ExprId, depth: u32) -> Option<String> {
    if depth > 2 {
        return None;
    }
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    match body.expr(expr) {
        Expr::Literal(Literal::Bool(true)) => Some("true".to_owned()),
        Expr::Literal(Literal::Int(0, _) | Literal::Uint(0, _)) => Some("0".to_owned()),
        Expr::Tuple { exprs, .. } if exprs.is_empty() => Some("()".to_owned()),
        Expr::Block { statements, tail: None, .. } if statements.iter().next().is_none() => {
            Some("{}".to_owned())
        }
        Expr::Path(_) => {
            let variant = success_variant(cx, infer.variant_resolution_for_expr(expr)?)?;
            (variant == "None").then(|| variant.to_owned())
        }
        Expr::Call { callee, args, .. } => {
            if let Some(variant) = infer.variant_resolution_for_expr(callee) {
                let variant = success_variant(cx, variant)?;
                if variant == "None" {
                    return None;
                }
                let [arg] = args else { return None };
                let inner = constant(cx, analysis, *arg, depth + 1)?;
                return Some(format!("{variant}({inner})"));
            }
            if !args.is_empty() {
                return None;
            }
            let (f, _) = analysis.callee(expr)?;
            let name = hir::Function::from(f).name(db);
            matches!(name.as_str(), "new" | "default" | "empty").then(|| format!("{}()", name.as_str()))
        }
        _ => None,
    }
}

/// The name of `variant` when it is `Ok`, `Some` or `None` of core's `Result` or `Option`.
fn success_variant(cx: &Cx<'_, '_>, variant: VariantId) -> Option<&'static str> {
    let db = cx.db();
    let VariantId::EnumVariantId(id) = variant else { return None };
    let variant = hir::EnumVariant::from(id);
    let famous = cx.famous();
    let parent = Some(variant.parent_enum(db));
    let name = variant.name(db);
    if parent == famous.core_result_Result() && name.as_str() == "Ok" {
        return Some("Ok");
    }
    if parent == famous.core_option_Option() {
        return match name.as_str() {
            "Some" => Some("Some"),
            "None" => Some("None"),
            _ => None,
        };
    }
    None
}

/// The inputs of `f` a body could read: its parameters, and `self` unless its type is a struct
/// with no fields.
fn inputs(cx: &Cx<'_, '_>, f: ast::Fn<'_>, func: hir::Function) -> Vec<String> {
    let db = cx.db();
    // alloc: the names of one function's parameters, a few
    let mut names = Vec::new();
    let Some(list) = f.param_list() else { return names };
    if list.self_param().is_some() {
        let empty = func
            .self_param(db)
            .and_then(|it| it.ty(db).strip_references().as_adt())
            .is_some_and(|adt| matches!(adt, hir::Adt::Struct(it) if it.fields(db).is_empty()));
        if !empty {
            names.push("`self`".to_owned());
        }
    }
    for param in list.params() {
        let text = param.pat().map_or_else(|| "_".to_owned(), |it| cx.source_of(it.syntax()).into_owned());
        names.push(format!("`{text}`"));
    }
    names
}

/// The first word of `f`'s doc that promises an effect.
fn promise(f: ast::Fn<'_>) -> Option<&'static str> {
    for doc in f.doc_comments() {
        let text = doc.syntax().text();
        for word in text.split(|it: char| !it.is_ascii_alphabetic()) {
            let word = word.to_ascii_lowercase();
            if let Some(it) = EFFECTS.iter().find(|it| **it == word) {
                return Some(it);
            }
        }
    }
    None
}
