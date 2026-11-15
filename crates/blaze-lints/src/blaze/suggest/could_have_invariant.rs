//! `could_have_invariant`: a struct whose functions check the same condition on the way in: two
//! or more functions of one inherent impl, at least one a constructor, whose body starts with
//! the same `assert!` or `debug_assert!`. The condition is the type's invariant, written once
//! per place a value is made or changed, and every place that makes one without it (a new
//! constructor, a `Default`, a struct literal in the module) breaks it with nothing to say so.
//!
//! `#[blaze::invariant(..)]` on the struct states the condition once: blaze's prover shows it at
//! every site that builds or writes a value and assumes it where one is read
//! (`hir_ty_pure::refined`), so a constructor added later cannot skip it and a caller may rely
//! on it. An integer field whose condition is a range is a ranged integer (`Int<LO, HI>`),
//! which carries the range in its type.
//!
//! A condition is compared by its tokens, whitespace and comments left out. A constructor is a
//! function with no `self` whose return type names `Self` or the struct; the other sites are
//! methods taking `&mut self`. Not found: a struct with a `pub` field (a caller can break any
//! invariant through it), a trait's impl, test code, and a struct that already has a
//! `#[blaze::invariant]`.
//!
//! Class: cheap (per impl, from its tree alone). No fix: the attribute's condition is in terms
//! of the fields (`self.lo <= self.hi`), the assertion's in terms of the parameters.

use hir::HasVisibility as _;
use hir_def::{AttrDefId, attrs::AttrFlags};
use syntax::{
    NodeOrToken, TextSize,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::super::{Lint, std_mistakes, test_code};
use crate::check::Cx;

const LINT: Lint = Lint::CouldHaveInvariant;

/// One function of the impl that starts with an assertion.
struct Site<'t> {
    f: ast::Fn<'t>,
    constructor: bool,
    /// The condition's tokens, trivia left out: what sites are compared by.
    condition: String,
    /// The condition as this site writes it.
    written: &'t str,
    /// The macro, as written: `assert` or `debug_assert`.
    mac: &'t str,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, imp: ast::Impl<'t>) {
    if !cx.on_blaze(LINT) || imp.trait_().is_some() || test_code::is_test_code(cx, imp.syntax()) {
        return;
    }
    let db = cx.db();
    let Some(hir::Adt::Struct(item)) = cx.impl_def(imp).and_then(|it| it.self_ty(db).as_adt()) else { return };
    if item.module(db).krate(db) != cx.krate()
        || item.fields(db).iter().any(|it| matches!(it.visibility(db), hir::Visibility::Public))
        || !AttrFlags::blaze_invariants(db, AttrDefId::AdtId(hir_def::AdtId::StructId(item.into()))).is_empty()
    {
        return;
    }
    let name = item.name(db).as_str().to_owned();
    let Some(list) = imp.assoc_item_list() else { return };
    // alloc: the impl's few asserting functions, once per impl linted
    let mut sites: Vec<Site<'t>, std::alloc::Global> = Vec::new_in(std::alloc::Global);
    for assoc in list.assoc_items() {
        let ast::AssocItem::Fn(f) = assoc else { continue };
        let self_param = f.param_list().and_then(|it| it.self_param());
        let constructor = self_param.is_none()
            && f.ret_type().is_some_and(|it| {
                let text = it.syntax().text();
                text.contains("Self") || text.split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == name)
            });
        let mutator = self_param.is_some_and(|it| {
            let text = it.syntax().text();
            text.contains("&mut") || text.contains("& mut")
        });
        if !constructor && !mutator {
            continue;
        }
        let Some((mac, condition, written)) = first_assertion(f) else { continue };
        sites.push(Site { f, constructor, condition, written, mac });
    }
    let mut reported: Vec<&str, std::alloc::Global> = Vec::new_in(std::alloc::Global);
    for site in &sites {
        if !site.constructor || reported.contains(&site.condition.as_str()) {
            continue;
        }
        let same: Vec<&Site<'t>, std::alloc::Global> = {
            let mut it = Vec::new_in(std::alloc::Global);
            it.extend(sites.iter().filter(|other| other.condition == site.condition));
            it
        };
        if same.len() < 2 {
            continue;
        }
        reported.push(site.condition.as_str());
        let names: Vec<String, std::alloc::Global> = {
            let mut it = Vec::new_in(std::alloc::Global);
            it.extend(same.iter().filter_map(|it| it.f.name()).map(|it| format!("`{}`", it.syntax().text())));
            it
        };
        let message = format!(
            "{} each start with `{}!({})`: that is an invariant of `{name}`; state it once with \
             `#[blaze::invariant(..)]` on `{name}`",
            names.join(", "),
            site.mac,
            site.written,
        );
        let note = "blaze proves a `#[blaze::invariant]` at every site that builds or writes a value and assumes \
                    it where one is read, so a constructor added later cannot skip it and a caller may rely on it; \
                    a field whose condition is a range can be a ranged integer (`Int<LO, HI>`) instead";
        let Some(range) = site.f.name().and_then(|it| cx.range(it.syntax())) else { continue };
        std_mistakes::report(cx, LINT, site.f.syntax(), range, message, note, None);
    }
}

/// The macro and condition of `f`'s first statement, when it is `assert!(cond)` or
/// `debug_assert!(cond)` (a message after the condition left out).
fn first_assertion<'t>(f: ast::Fn<'t>) -> Option<(&'t str, String, &'t str)> {
    let stmt = f.body()?.stmt_list()?.statements().next()?;
    let ast::Stmt::ExprStmt(stmt) = stmt else { return None };
    let ast::Expr::MacroExpr(mac) = stmt.expr()? else { return None };
    let call = mac.macro_call()?;
    let name = call.path()?.segment()?.name_ref()?.syntax().text();
    if !matches!(name, "assert" | "debug_assert") {
        return None;
    }
    // The condition's tokens: inside the outer parentheses, up to the first comma outside
    // brackets (what follows it is the message), trivia left out. The key separates tokens by
    // a byte no token holds, so `a b` and `ab` differ; the written form is the source between
    // the first token and the last.
    let tree = call.token_tree()?.syntax();
    let mut depth = 0i32;
    let mut seen = false;
    let mut span: Option<(TextSize, TextSize)> = None;
    // alloc: one condition's key, compared across the impl's functions
    let mut key = String::new();
    for it in tree.descendants_with_tokens() {
        let NodeOrToken::Token(token) = it else { continue };
        if token.kind().is_trivia() {
            continue;
        }
        let text = token.text();
        match text {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            _ => {}
        }
        let outer = (!seen && depth == 1 && text == "(") || (depth == 0 && text == ")");
        seen = true;
        if depth == 1 && text == "," {
            break;
        }
        if outer {
            continue;
        }
        key.push_str(text);
        key.push('\u{1}');
        let range = token.text_range();
        span = Some((span.map_or(range.start(), |it| it.0), range.end()));
    }
    let (start, end) = span?;
    let base = tree.text_range().start();
    let written = tree.text().get(usize::from(start - base)..usize::from(end - base))?;
    Some((name, key, written))
}
