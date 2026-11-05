//! `outline_generic_body`: a generic function whose type parameters are used only by a thin
//! conversion at its entry (`let path = path.as_ref();`, `let name = name.into();`,
//! `let items = items.into_iter().collect::<Vec<u32>>();`, an `impl Fn` called once), while the
//! rest of its body, over [`REST`] expressions, uses only what the conversions made. Every
//! instantiation copies the whole body, though only the conversion differs between them; moved
//! into a non-generic `fn inner(..)` the generic shell calls (the "momo" pattern), the body is
//! compiled once and each instantiation is the conversion and one call.
//!
//! Found when, for a function (not a closure, not `async`, not under `#[cfg(test)]`):
//!
//! - each of its own type parameters is the type of exactly one value parameter (`p: P`,
//!   `p: &P`, `p: impl AsRef<Path>`), and it has no const parameters;
//! - each such parameter, a plain binding, is named once in the body, as the receiver of a
//!   conversion of [`CONVERSIONS`] (or `into_iter()` then `collect()`), the argument of a
//!   one-argument `from`/`into`/`as_ref`/.. call, or the callee of a call, and that conversion is
//!   the initializer of one of the body's first statements, which are those `let`s and nothing
//!   else;
//! - nothing past them is generic: no expression's type, no callee's generic arguments and no
//!   return type names a type parameter, and no identifier of the body is a named type
//!   parameter (`T::default()`, `size_of::<T>()`);
//! - the rest of the body has more than [`REST`] expressions.
//!
//! The fix, machine-applicable, is offered only where the rewrite keeps the meaning: a free
//! function (no `self`, no `Self`, not in an impl or trait), not `const`, `unsafe` or `extern`,
//! written in the file, whose return type is written without a reference, a lifetime or `impl`
//! (so elision in the inner signature means what it did), whose conversions read no other local,
//! whose parameters and `let`s are plain names that don't clash, whose conversion types print
//! as source with no lifetime, and whose body names nothing `inner`. It nests
//! `fn inner(..) -> R { <rest> }` in the body after the conversions' `let`s; a `return` or `?`
//! in the rest returns the same type from `inner`, which the shell returns. Every other finding
//! gets help only. Keep one with `#[expect(blaze::outline_generic_body, reason = "..")]`.

use base_db::Db;
use hir::{AsAssocItem as _, HirDisplay as _};
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{Expr, ExprId, Pat, Statement},
    signatures::FunctionSignature,
};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{Lint, alloc::written_callee, facts::Parents, hash_order_output::local_of};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::OutlineGenericBody;

/// 30 HIR expressions past the conversions: about five to eight lines of code. Below that the
/// body is small enough that LLVM inlines it into its callers, so its copies cost about what a
/// shell and a call would; above it, each instantiation carries a full copy of the body's
/// machine code (a few instructions per expression, so hundreds of bytes per copy) and its
/// compile time, while a shell over an outlined body is a conversion and a call.
const REST: usize = 30;

/// The expressions a body may have past its conversions: `outline-generic-body-threshold` in
/// `clippy.toml`, or [`REST`].
fn rest_limit(cx: &Cx<'_, '_>) -> usize {
    usize::try_from(cx.conf().int("outline-generic-body-threshold", REST as i64)).unwrap_or(REST)
}

/// The methods a parameter's one use may be: each converts it to a concrete type once.
const CONVERSIONS: &[&str] = &[
    "as_ref",
    "as_mut",
    "borrow",
    "borrow_mut",
    "into",
    "to_string",
    "to_owned",
    "as_str",
    "as_slice",
    "as_bytes",
    "as_path",
    "as_os_str",
];

/// The name the fix gives the outlined function.
const INNER: &str = "inner";

/// A finding, before it is reported.
struct Found {
    range: TextRange,
    message: String,
    note: String,
    fix: Option<(TextRange, String)>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(found) = find(cx) else { return };
    let root = cx.root();
    let at = match root.text_range().contains_range(found.range) {
        true => match root.covering_element(found.range) {
            NodeOrToken::Node(it) => it,
            NodeOrToken::Token(it) => it.parent().unwrap_or(root),
        },
        false => root,
    };
    let span = cx.span(found.range);
    let fix = found.fix.map(|(range, text)| (cx.span(range), text));
    let diagnostic = cx.blaze(LINT, at, span, found.message);
    diagnostic.note(found.note);
    if let Some(part) = fix {
        // alloc: the fix's one part
        diagnostic.multipart_suggestion(
            "outline the body into a non-generic `fn inner`",
            vec![part],
            Applicability::MachineApplicable,
        );
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// One parameter of a type parameter, and the `let` its one conversion initializes.
struct Converted {
    /// Its index among the value parameters, `self` not counted.
    param: usize,
    /// The conversion: the `let`'s initializer.
    conversion: ExprId,
    /// The `let`'s index among the body's statements.
    statement: usize,
    /// The `let`'s pattern.
    pat: hir_def::hir::PatId,
}

fn find(cx: &Cx<'_, '_>) -> Option<Found> {
    let f = ast::Fn::cast(cx.root())?;
    if cx.expansion().is_some()
        || !cx.is_written(f.syntax())
        || f.async_token().is_some()
        || cx.is_in_test(f.syntax())
    {
        return None;
    }
    let db = cx.db();
    let analysis = cx.analysis()?;
    let hir::DefWithBody::Function(func) = analysis.def() else { return None };
    let def = DefWithBodyId::try_from(analysis.def()).ok()?;
    if func.is_async(db) {
        return None;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    // Cheapest first: a body too small to be worth outlining is left before any resolution.
    let total = body.exprs().count();
    let limit = rest_limit(cx);
    if total <= limit {
        return None;
    }
    // alloc: the function's few type parameters, once per generic function
    let mut own = Vec::new();
    for param in hir::GenericDef::Function(func).params(db) {
        match param {
            hir::GenericParam::TypeParam(it) => own.push(it),
            hir::GenericParam::ConstParam(_) => return None,
            hir::GenericParam::LifetimeParam(_) => {}
        }
    }
    if own.is_empty() {
        return None;
    }
    let values = func.params_without_self(db);
    if values.len() != body.params.len() {
        return None;
    }
    // Each type parameter is the type of exactly one value parameter, references peeled.
    // alloc: the few generic parameters, once per generic function
    let mut generic: Vec<(usize, hir::TypeParam)> = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let ty = value.ty();
        if hir_ty::collect_params(&ty_of(ty)).is_empty() {
            continue;
        }
        let param = ty.strip_references().as_type_param(db)?;
        if !own.contains(&param) || generic.iter().any(|(_, it)| *it == param) {
            return None;
        }
        generic.push((index, param));
    }
    if generic.len() != own.len() {
        return None;
    }
    let Expr::Block { statements, .. } = body.expr(body.root_expr()) else { return None };
    let parents = Parents::of(body);
    // alloc: one entry per generic parameter
    let mut converted: Vec<Converted> = Vec::new();
    for &(index, _) in &generic {
        let pat = body.params[index].formal;
        let Pat::Bind { id: binding, subpat: None } = body.pat(pat) else { return None };
        let mut uses = body.exprs().filter(|(id, expr)| {
            matches!(expr, Expr::Path(_)) && local_of(db, def, body, *id) == Some(binding)
        });
        let (use_, _) = uses.next()?;
        if uses.next().is_some() {
            return None;
        }
        let conversion = conversion(db, body, infer, &parents, use_)?;
        let (statement, let_pat) = statements.iter().enumerate().find_map(|(at, it)| match it {
            Statement::Let { pat, initializer: Some(init), else_branch: None, .. }
                if init == conversion =>
            {
                Some((at, pat))
            }
            _ => None,
        })?;
        if !matches!(body.pat(let_pat), Pat::Bind { subpat: None, .. }) {
            return None;
        }
        // What the conversion makes is concrete.
        if !hir_ty::collect_params(&infer.expr_ty(conversion)).is_empty() {
            return None;
        }
        converted.push(Converted { param: index, conversion, statement, pat: let_pat });
    }
    // The conversions are the body's first statements and nothing else is.
    let mut at: Vec<usize> = converted.iter().map(|it| it.statement).collect(); // alloc: a few indices
    at.sort_unstable();
    if at.iter().enumerate().any(|(i, &it)| i != it) {
        return None;
    }
    // alloc: the expressions of the conversions, once per candidate
    let mut prefix = Vec::new();
    for it in &converted {
        subtree(body, it.conversion, &mut prefix);
    }
    let rest = total.saturating_sub(prefix.len() + 1);
    if rest <= limit {
        return None;
    }
    // Nothing past the conversions is generic.
    if !hir_ty::collect_params(&ty_of(&func.ret_type(db))).is_empty() {
        return None;
    }
    for (id, expr) in body.exprs() {
        if prefix.contains(&id) || id == body.root_expr() {
            continue;
        }
        if !hir_ty::collect_params(&infer.expr_ty(id)).is_empty() {
            return None;
        }
        if matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. })
            && let Some((_, args)) = analysis.callee(id)
            && !hir_ty::collect_params(&args).is_empty()
        {
            return None;
        }
    }
    let names = |it: &hir::TypeParam| (!it.is_implicit(db)).then(|| it.name(db));
    let named: Vec<hir::Name> = own.iter().filter_map(names).collect(); // alloc: a few names
    if let Some(block) = f.body()
        && block.syntax().descendants_with_tokens().filter_map(|it| it.into_token()).any(|it| {
            it.kind() == SyntaxKind::IDENT && named.iter().any(|name| name.as_str() == it.text())
        })
    {
        return None;
    }
    let name = func.name(db);
    let name = name.as_str();
    let range = cx.range(f.name().map_or(f.syntax(), |it| it.syntax()))?;
    let params = converted
        .iter()
        .map(|it| match values[it.param].name(db) {
            Some(name) => format!("`{}`", name.as_str()),
            None => "a parameter".to_owned(), // alloc: a message's part
        })
        .collect::<Vec<_>>() // alloc: a message's parts
        .join(", ");
    let message = format!(
        "`fn {name}` is generic only in how it converts {params} at entry: the {rest} expressions after the \
         conversion are copied into every instantiation"
    );
    let note = format!(
        "move what follows the conversion into a non-generic `fn {INNER}(..)` that `{name}` calls with the converted \
         values (the \"momo\" pattern): the body is then compiled once, and each instantiation is the conversion and \
         one call"
    );
    let fix = fix(cx, f, func, analysis, def, &converted);
    Some(Found { range, message, note, fix })
}

/// The type of `ty`, for [`hir_ty::collect_params`].
fn ty_of<'db>(ty: &hir::Type<'db>) -> hir_ty::next_solver::Ty<'db> {
    ty.to_next_solver()
}

/// The conversion the use `use_` of a parameter is the start of, if it is one.
fn conversion(
    db: &Db,
    body: &Body,
    infer: &hir_ty::InferenceResult<'_>,
    parents: &Parents,
    use_: ExprId,
) -> Option<ExprId> {
    let parent = parents.of_expr(use_)?;
    match body.expr(parent) {
        Expr::MethodCall { receiver, method_name, args, .. }
            if receiver == use_ && args.is_empty() =>
        {
            let method = method_name.as_str();
            if method == "into_iter" {
                let up = parents.of_expr(parent)?;
                return match body.expr(up) {
                    Expr::MethodCall { receiver, method_name, args, .. }
                        if receiver == parent
                            && args.is_empty()
                            && method_name.as_str() == "collect" =>
                    {
                        Some(up)
                    }
                    _ => None,
                };
            }
            CONVERSIONS.contains(&method).then_some(parent)
        }
        // An `impl Fn` called once.
        Expr::Call { callee, .. } if callee == use_ => Some(parent),
        Expr::Call { args: [arg], .. } if *arg == use_ => {
            let (callee, _) = written_callee(infer, body, parent)?;
            let called = FunctionSignature::of(db, callee).name.as_str();
            (called == "from" || CONVERSIONS.contains(&called)).then_some(parent)
        }
        _ => None,
    }
}

/// `expr` and every expression under it, into `out`.
fn subtree(body: &Body, expr: ExprId, out: &mut Vec<ExprId>) {
    // alloc: the worklist of one conversion
    let mut stack = vec![expr];
    while let Some(it) = stack.pop() {
        out.push(it);
        body.walk_child_exprs(it, |child| stack.push(child));
    }
}

/// The body rewritten with the rest outlined, when that is sure to mean the same: `converted`
/// are the body's first `converted.len()` statements.
fn fix<'t>(
    cx: &Cx<'_, 't>,
    f: ast::Fn<'t>,
    func: hir::Function,
    analysis: &hir::BodyAnalysis<'_>,
    def: DefWithBodyId,
    converted: &[Converted],
) -> Option<(TextRange, String)> {
    let db = cx.db();
    let n = converted.len();
    if !cx.in_file() || func.as_assoc_item(db).is_some() || func.has_self_param(db) {
        return None;
    }
    if f.const_token().is_some()
        || f.unsafe_token().is_some()
        || f.abi().is_some()
        || f.gen_token().is_some()
    {
        return None;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    let Expr::Block { statements, tail, .. } = body.expr(body.root_expr()) else { return None };
    // Each conversion reads its own parameter and no other local.
    for it in converted {
        let Pat::Bind { id: own, .. } = body.pat(body.params[it.param].formal) else { return None };
        // alloc: one conversion's expressions
        let mut exprs = Vec::new();
        subtree(body, it.conversion, &mut exprs);
        if exprs.iter().any(|&e| local_of(db, def, body, e).is_some_and(|b| b != own)) {
            return None;
        }
    }
    let ret = match f.ret_type() {
        Some(ret) => {
            let text = cx.source_of(ret.syntax());
            if text.contains('&')
                || text.contains('\'')
                || text.contains("impl")
                || text.contains("Self")
            {
                return None;
            }
            format!(" {text}")
        }
        None => String::new(),
    };
    let block = f.body()?;
    let list = block.stmt_list()?;
    let stmts: Vec<ast::Stmt<'t>> = list.statements().collect(); // alloc: the body's statements, once per fix
    if stmts.len() != statements.len()
        || stmts[..n].iter().any(|it| !matches!(it, ast::Stmt::LetStmt(_)))
    {
        return None;
    }
    let tail_present = tail.is_some();
    let list_range = cx.range(list.syntax())?;
    let inner_start = list_range.start() + syntax::TextSize::of('{');
    let inner_end = list_range.end().checked_sub(syntax::TextSize::of('}'))?;
    let prefix_end = cx.range(stmts.get(n.checked_sub(1)?)?.syntax())?.end();
    let rest_start = match (stmts.get(n), list.tail_expr()) {
        (Some(it), _) => cx.range(it.syntax())?.start(),
        (None, Some(tail)) if tail_present => cx.range(tail.syntax())?.start(),
        _ => return None,
    };
    if rest_start < prefix_end
        || !cx.snippet(TextRange::new(prefix_end, rest_start)).trim().is_empty()
    {
        return None;
    }
    let module = hir_def::ModuleId::from(func.module(db));
    // alloc: the names the inner function takes, once per fix
    let mut names: Vec<String> = Vec::new();
    let mut inner_params: Vec<String> = Vec::new(); // alloc: as above
    for it in converted {
        let ast::Stmt::LetStmt(stmt) = stmts[it.statement] else { return None };
        let ast::Pat::IdentPat(pat) = stmt.pat()? else { return None };
        if pat.ref_token().is_some() || pat.pat().is_some() {
            return None;
        }
        let ty = analysis.ty(infer.pat_ty(it.pat));
        if ty.contains_unknown() {
            return None;
        }
        let ty = ty.display_source_code(db, module, false).ok()?;
        if ty.contains('\'') || ty.contains("impl ") || ty.contains('{') {
            return None;
        }
        names.push(pat.name()?.text().to_owned()); // alloc: a name
        inner_params.push(format!("{}: {ty}", cx.source_of(pat.syntax())));
    }
    let generic: Vec<usize> = converted.iter().map(|it| it.param).collect(); // alloc: a few indices
    for (index, param) in f.param_list()?.params().enumerate() {
        if generic.contains(&index) {
            continue;
        }
        let ast::Pat::IdentPat(pat) = param.pat()? else { return None };
        if pat.ref_token().is_some() || pat.pat().is_some() {
            return None;
        }
        names.push(pat.name()?.text().to_owned()); // alloc: a name
        inner_params.push(cx.source_of(param.syntax()).into_owned());
    }
    let distinct = names.iter().enumerate().all(|(i, a)| names[..i].iter().all(|b| b != a));
    let clash = block
        .syntax()
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .any(|it| it.kind() == SyntaxKind::IDENT && it.text() == INNER);
    if !distinct || clash || names.iter().any(|it| it == INNER) {
        return None;
    }
    let prefix = cx.snippet(TextRange::new(inner_start, prefix_end));
    let rest = cx.snippet(TextRange::new(rest_start, inner_end)).trim_end();
    let text = format!(
        "{{\n    fn {INNER}({}){ret} {{\n        {rest}\n    }}{prefix}\n    {INNER}({})\n}}",
        inner_params.join(", "),
        names.join(", "),
    );
    Some((list_range, text))
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, as_ref, from, option, add
//- /src/lib.rs crate:main
pub struct Path { bytes: u32 }
impl AsRef<Path> for Path { fn as_ref(&self) -> &Path { self } }
fn step(p: &Path, n: u32) -> u32 { p.bytes + n }
pub fn open<P: AsRef<Path>>(path: P) -> u32 {
    let path = path.as_ref();
    let a = step(path, 1) + step(path, 2) + step(path, 3);
    let b = step(path, a) + step(path, a + 1) + step(path, a + 2);
    let c = step(path, b) + step(path, b + 1) + step(path, b + 2);
    a + b + c + step(path, a + b + c)
}
pub fn small<P: AsRef<Path>>(path: P) -> u32 {
    let path = path.as_ref();
    step(path, 1)
}
pub fn kept<P: AsRef<Path>>(path: P) -> u32 {
    let first = path.as_ref();
    let a = step(first, 1) + step(first, 2) + step(first, 3);
    let b = step(first, a) + step(first, a + 1) + step(first, a + 2);
    let c = step(path.as_ref(), b) + step(first, b + 1) + step(first, b + 2);
    a + b + c + step(first, a + b + c)
}
"#;

    #[test]
    fn a_body_generic_only_in_its_conversion_is_found_with_a_fix() {
        let found = lint(FIXTURE.trim_start(), "-A clippy::all --warn=blaze::outline_generic_body");
        assert!(
            found.contains("`fn open` is generic only in how it converts `path` at entry"),
            "{found}"
        );
        assert!(
            found.contains("fn inner(path: &Path) -> u32 {"),
            "the fix outlines the body:\n{found}"
        );
        assert!(!found.contains("`fn small`"), "a small body is left alone:\n{found}");
        assert!(!found.contains("`fn kept`"), "a parameter used twice is left alone:\n{found}");
    }
}
