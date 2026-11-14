//! `unseen_target`: a conversion or a builder whose result type nobody wrote, so the reader (and
//! the next edit) has to find it from how the value is used further down. `let n = s.parse()?;`
//! is a `u32` because three lines later `n` is handed to a function of one; change that call and
//! the parse quietly parses something else.
//!
//! The rule, by resolution: a call of `Into::into`, `TryInto::try_into`, `str::parse`,
//! `Iterator::collect`, `Iterator::sum`, `Iterator::product` (each of `core`, `alloc` or `std`)
//! with no turbofish, or `Default::default()` called by the trait's path, whose value (past
//! parentheses, a `?`, an `.unwrap()` or an `.expect(..)`) is the initializer of a `let` with no
//! type written. Nothing at the statement names the type: it comes from a later use. A call whose
//! value is an argument, a returned value, a field of a struct literal or an assignment to a
//! typed place is not found: the type is written there, one step away, in the signature or the
//! declaration it meets.
//!
//! The fix writes the type the inference found, machine-applicable where the type is written
//! without a path (`u32`, `Vec<u32>`, `String`), maybe-incorrect where it names one: for
//! `parse`, `collect`, `sum` and `product` the turbofish (`.parse::<u32>()`,
//! `.collect::<Vec<u32>>()`), which holds through a `?`; for `into`, `try_into` and
//! `Default::default()`, which take none, the `let`'s type (`let n: u64 = m.into();`). A type
//! with a lifetime, a closure or an `impl` in it, or one inference left unknown, gets no fix.

use hir::{AsAssocItem as _, HirDisplay as _};
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasName as _},
};

use super::super::{
    Lint, facts,
    std_mistakes::{Fix, report},
};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::UnseenTarget;

/// What a call is and how its fix writes the type.
#[derive(Clone, Copy, PartialEq)]
enum Write {
    /// The turbofish of the method, whose generic is the call's own type.
    Turbofish,
    /// The turbofish of `parse`, whose generic is the `Ok` of the call's `Result`.
    ParseTurbofish,
    /// The type of the `let`'s pattern.
    LetType,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !matches!(node.kind(), SyntaxKind::METHOD_CALL_EXPR | SyntaxKind::CALL_EXPR)
        || !cx.on_blaze(LINT)
        || cx.analysis().is_none()
    {
        return;
    }
    let Some((what, write)) = target_generic(cx, node) else { return };
    let Some(stmt) = untyped_let_of(cx, node) else { return };
    let Some(pat) = stmt.pat() else { return };
    let Some(range) = cx.range(node) else { return };
    let bound = match pat {
        ast::Pat::IdentPat(it) => it.name().map(|it| format!("`{}`", it.text())),
        _ => None,
    }
    .unwrap_or_else(|| "the value".to_owned());
    let message = format!(
        "the type {what} makes is written nowhere: only how {bound} is used later fixes it"
    );
    let note = "write the type where the value is made (a turbofish or the `let`'s type), so the reader sees it \
                and a later edit can't change it quietly";
    let fix = fix_of(cx, node, pat, write);
    report(cx, LINT, node, range, message, note, fix);
}

/// Which target-generic call `node` is, as its message names it, and how its fix writes the
/// type; `None` when it is none of them or its type is written at the call.
fn target_generic<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<(&'static str, Write)> {
    let db = cx.db();
    if let Some(call) = ast::MethodCallExpr::cast(node) {
        if call.generic_arg_list().is_some() {
            return None;
        }
        let f = cx.method(call)?;
        if !of_std(cx, f.module(db).krate(db)) {
            return None;
        }
        let of_trait = f
            .as_assoc_item(db)
            .and_then(|it| it.container_or_implemented_trait(db))
            .map(|it| it.name(db));
        let iterator = cx.famous().core_iter_Iterator();
        let of_iterator = f
            .as_assoc_item(db)
            .and_then(|it| it.container_or_implemented_trait(db))
            .is_some_and(|it| Some(it) == iterator);
        return match (f.name(db).as_str(), of_trait.as_ref().map(|it| it.as_str())) {
            ("into", Some("Into")) => Some(("`.into()`", Write::LetType)),
            ("try_into", Some("TryInto")) => Some(("`.try_into()`", Write::LetType)),
            ("parse", None) => Some(("`.parse()`", Write::ParseTurbofish)),
            ("collect", _) if of_iterator => Some(("`.collect()`", Write::Turbofish)),
            ("sum", _) if of_iterator => Some(("`.sum()`", Write::Turbofish)),
            ("product", _) if of_iterator => Some(("`.product()`", Write::Turbofish)),
            _ => None,
        };
    }
    let call = ast::CallExpr::cast(node)?;
    let ast::Expr::PathExpr(callee) = call.expr()?.peel_parens() else { return None };
    let path = callee.path()?;
    let qualifier = path.qualifier()?;
    let Some(hir::PathResolution::Def(hir::ModuleDef::Trait(trait_))) = cx.path(qualifier) else {
        return None;
    };
    let Some(hir::PathResolution::Def(hir::ModuleDef::Function(f))) = cx.path(path) else {
        return None;
    };
    (trait_.name(db).as_str() == "Default"
        && of_std(cx, trait_.module(db).krate(db))
        && f.name(db).as_str() == "default")
        .then_some(("`Default::default()`", Write::LetType))
}

/// Whether `krate` is `core`, `alloc` or `std`.
fn of_std(cx: &Cx<'_, '_>, krate: hir::Crate) -> bool {
    ["core", "alloc", "std"].iter().any(|it| facts::crate_is(cx.db(), krate, it))
}

/// The `let` with no type `node`'s value initializes, past parentheses, a `?`, an `.unwrap()`
/// and an `.expect(..)`.
fn untyped_let_of<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<ast::LetStmt<'t>> {
    let mut at = node;
    for _ in 0..6 {
        let mut parent = at.parent()?;
        while parent.kind() == SyntaxKind::PAREN_EXPR {
            parent = parent.parent()?;
        }
        match parent.kind() {
            SyntaxKind::TRY_EXPR => at = parent,
            SyntaxKind::METHOD_CALL_EXPR => {
                let call = ast::MethodCallExpr::cast(parent)?;
                if call.receiver()?.peel_parens().syntax() != at {
                    return None;
                }
                let f = cx.method(call)?;
                if !matches!(f.name(cx.db()).as_str(), "unwrap" | "expect")
                    || !of_std(cx, f.module(cx.db()).krate(cx.db()))
                {
                    return None;
                }
                at = parent;
            }
            SyntaxKind::LET_STMT => {
                let stmt = ast::LetStmt::cast(parent)?;
                let value = stmt.initializer()?.peel_parens();
                return (value.syntax() == at && stmt.ty().is_none()).then_some(stmt);
            }
            _ => return None,
        }
    }
    None
}

/// The fix writing the type `write` says where it says.
fn fix_of<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>, pat: ast::Pat<'t>, write: Write) -> Option<Fix> {
    let db = cx.db();
    let module = hir_def::ModuleId::from(cx.analysis()?.def().module(db));
    let (ty, at) = match write {
        Write::Turbofish | Write::ParseTurbofish => {
            let call = ast::MethodCallExpr::cast(node)?;
            let name = call.name_ref()?;
            let ty = cx.ty(ast::Expr::cast(node)?)?;
            let ty = match write {
                Write::ParseTurbofish => ty.type_arguments().next()?,
                _ => ty,
            };
            (ty, cx.range(name.syntax())?.end())
        }
        Write::LetType => (cx.pat_ty(pat)?, cx.range(pat.syntax())?.end()),
    };
    if ty.contains_unknown() {
        return None;
    }
    let shown = ty.display_source_code(db, module, false).ok()?;
    if shown.replace("'static", "").contains('\'') || shown.contains('{') || shown.contains("impl ") {
        return None;
    }
    let applicability = match shown.contains("::") {
        true => Applicability::MaybeIncorrect,
        false => Applicability::MachineApplicable,
    };
    let text = match write {
        Write::LetType => format!(": {shown}"),
        _ => format!("::<{shown}>"),
    };
    // alloc: the fix's one part
    Some(Fix {
        message: "write the type here",
        parts: vec![(TextRange::empty(at), text)],
        applicability,
    })
}
