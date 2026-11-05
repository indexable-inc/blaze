//! `native_contracts`: a function of a crate with `#![feature(blaze_verify)]` that states its
//! contract with nightly's `#[core::contracts::requires(..)]` / `#[core::contracts::ensures(..)]`,
//! which blaze_verify's native clauses say directly in the signature. One function takes one
//! form (hir-def refuses both on one function), and the native one is what blaze verifies.
//!
//! The attributes are found by what name resolution made of them, never by their text: each
//! attribute macro call between the file and the function's body ([`Cx::attr_calls`]) calls a
//! macro, and the lint fires when one of them is the builtin `ContractsRequires` or
//! `ContractsEnsures`, through `use`, renames, globs and re-exports.
//!
//! The fix keeps what the contract states:
//!
//! - `#[requires(P)]` is `requires P`, and `#[requires(DECLS; P)]` is `requires ({ DECLS; P })`.
//! - `#[ensures(|PAT| E)]` names the return value, `-> T` becoming `-> (r: T)`, and is
//!   `ensures ({ let PAT = &r; E })`: the closure's parameter bound as nightly binds it, to a
//!   reference to the value returned, any pattern and type written on it kept. A closure whose
//!   body can leave it (`return`, `?`), that has a return type or a `for<..>` is called instead,
//!   `ensures ((CLOSURE)(&r))`. `#[ensures(path)]`, a function or a `const`, is
//!   `ensures path(&r)`.
//! - The name is the return's own when it has one, else the closure parameter's when it is a
//!   plain name no parameter of the function has, else the first of `ret`, `result`, `ret_`,
//!   `ret0` .. `ret63` that no identifier of the function uses. A function with no `->` returns
//!   `()`, so its value is `&()` and its signature stays as it is.
//! - Each clause is parenthesized where the clause grammar would not read it as one expression
//!   on its own (a block, which would end the clause list; a struct literal; a keyword).
//!
//! It is made only where it says the same thing. Nightly builds the ensures closure at the
//! function's start and runs it at its end, and the requires' declarations are in its scope; a
//! native `ensures` is read at the end and sees only the parameters. So there is no fix, with a
//! note saying why, when the ensures closure reads anything but a parameter that is neither `mut`
//! nor of a type that is not `Copy` (both could differ between start and end), reads a local of
//! the requires' declarations, or when the requires declares an item; when the ensures is any
//! other expression (it would run at the end, not the start); when a clause can leave through
//! `return` or `?` outside a closure; when another attribute macro also rewrites the function,
//! or an attribute is in a `cfg_attr` or made by a macro; when the function already has native
//! clauses; and when the return type has an `impl Trait`, which a named return can't hold yet.
//!
//! Not changed: the `use` that imported the attributes, which rustc's `unused_imports` then
//! reports. Checking: the native clauses are checked as the profile says (blaze_verify), where
//! nightly's are checked under `-Zcontract-checks`; the fix keeps what is stated, not when it
//! is checked.

use hir_expand::builtin::BuiltinAttrExpander;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize, WalkEvent,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::Lint;
use crate::{
    check::{AttrCall, Cx},
    diag::Applicability,
};

const LINT: Lint = Lint::NativeContracts;

/// How deep the macro calls in a clause are followed to find a `return` or `?` in them.
const MACRO_DEPTH: u32 = 8;

/// How many `retN` names are tried for the return value after `ret`, `result` and `ret_`.
const FRESH: u32 = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    Requires,
    Ensures,
}

impl Clause {
    fn of(call: &AttrCall) -> Option<Clause> {
        match call.builtin? {
            BuiltinAttrExpander::ContractsRequires => Some(Clause::Requires),
            BuiltinAttrExpander::ContractsEnsures => Some(Clause::Ensures),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Clause::Requires => "requires",
            Clause::Ensures => "ensures",
        }
    }
}

/// Lints `function`, the item of the body linted, when nightly's contract attributes wrote it.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, function: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) || function.syntax() != cx.root() || !cx.features().blaze_verify {
        return;
    }
    let Some((item, calls)) = cx.attr_calls() else { return };
    let mut written = Vec::new(); // alloc: the attributes named in the message, at most two
    let mut first = None;
    for call in &calls {
        let Some(clause) = Clause::of(call) else { continue };
        let path = call.written.and_then(|(attr, _)| written_path(cx.snippet(attr)));
        written.push(format!("`#[{}]`", path.unwrap_or(clause.name())));
        if first.is_none() {
            first = call.written.map(|(attr, _)| attr);
        }
    }
    if written.is_empty() {
        return;
    }
    let Some(at) = first.or(cx.call_at()) else { return };
    let name = function.name().map_or("_", |it| it.text());
    let message = format!(
        "`fn {name}` states its contract with nightly's {} in a crate with `#![feature(blaze_verify)]`, whose native clauses say it in the signature",
        written.join(" and ")
    );
    let fix = plan(cx, function, item, &calls).map(|parts| {
        parts.into_iter().map(|(range, text)| (cx.span(range), text)).collect::<Vec<_>>()
    }); // alloc: the fix's parts
    let span = cx.span(at);
    let diagnostic = cx.blaze(LINT, function.syntax(), span, message);
    match fix {
        Ok(parts) => {
            diagnostic.multipart_suggestion(
                "write the contract as native clauses",
                parts,
                Applicability::MachineApplicable,
            );
        }
        Err(why) => {
            diagnostic.note(format!("no fix is offered: {why}"));
        }
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// The path of the attribute `#[path(..)]` as written.
fn written_path(attr: &str) -> Option<&str> {
    let inner = attr.strip_prefix("#")?.trim_start().strip_prefix('[')?;
    Some(inner.split('(').next()?.trim()).filter(|it| !it.is_empty())
}

/// The marker statement hir-expand's contract expander leaves at the head of the body for each
/// clause, as hir-def's `lower/contract.rs` reads it: its block, and which clause.
fn marker(stmt: ast::Stmt<'_>) -> Option<(ast::BlockExpr<'_>, Clause)> {
    let ast::Stmt::ExprStmt(stmt) = stmt else { return None };
    let Some(ast::Expr::BlockExpr(block)) = stmt.expr() else { return None };
    block.attrs().find_map(|attr| match attr.simple_name()? {
        "rustc_contract_requires" => Some((block, Clause::Requires)),
        "rustc_contract_ensures" => Some((block, Clause::Ensures)),
        _ => None,
    })
}

/// One clause of the contract: its marker block in the expansion and its attribute as written.
struct Written<'t> {
    block: ast::BlockExpr<'t>,
    /// The attribute's `#[..]` and its `(..)`.
    attr: TextRange,
    args: TextRange,
}

/// The fix's parts, or why there is none.
fn plan<'t>(
    cx: &Cx<'_, 't>,
    function: ast::Fn<'t>,
    item: ast::Node<'t>,
    calls: &[AttrCall],
) -> Result<Vec<(TextRange, String)>, String> {
    let mut attrs = [None, None];
    for call in calls {
        let Some(clause) = Clause::of(call) else {
            return Err("another attribute macro rewrites it too, so its clauses can't be put back where it is written".into());
        };
        let Some(written) = call.written else {
            return Err(format!(
                "its `{}` attribute is made by a macro or held in a `cfg_attr`",
                clause.name()
            ));
        };
        let slot = &mut attrs[clause as usize];
        if slot.is_some() {
            return Err(format!("it has two `{}` attributes", clause.name()));
        }
        *slot = Some(written);
    }
    let source =
        ast::Fn::cast(item).ok_or("the item the attributes are on is not a function as written")?;
    if source.spec_clauses().next().is_some() {
        return Err("it has native clauses too".into());
    }
    let body = function.body().ok_or("it has no body")?;
    let source_body = source.body().ok_or("it has no body as written")?;

    // The marker each attribute left, matched to it by where its arguments are written.
    let mut clauses: [Option<Written<'t>>; 2] = [None, None];
    for stmt in body.statements() {
        let Some((block, clause)) = marker(stmt) else { break };
        let Some((attr, args)) = attrs[clause as usize] else {
            return Err(format!(
                "its body has a `{}` clause no attribute on it wrote",
                clause.name()
            ));
        };
        let inner = block.stmt_list().and_then(|it| cx.range(it.syntax()));
        if inner != Some(args) || clauses[clause as usize].is_some() {
            return Err(format!(
                "its `{}` clause is not where its attribute's arguments are",
                clause.name()
            ));
        }
        clauses[clause as usize] = Some(Written { block, attr, args });
    }
    for clause in [Clause::Requires, Clause::Ensures] {
        if attrs[clause as usize].is_some() && clauses[clause as usize].is_none() {
            return Err(format!(
                "its `{}` attribute left no clause in the body it expands to",
                clause.name()
            ));
        }
    }
    let [requires, ensures] = clauses;

    let mut parts = Vec::new(); // alloc: the fix's parts, a few
    let mut lines = Vec::new(); // alloc: the clauses' lines, at most two
    let mut declares_items = false;
    if let Some(requires) = &requires {
        if requires.block.tail_expr().is_none() {
            return Err("its `requires` has no condition after its declarations".into());
        }
        if escapes(cx, requires.block.syntax(), 0)? {
            return Err(
                "its `requires` can leave through `return` or `?`, which a native clause can't"
                    .into(),
            );
        }
        let has_decls = requires.block.statements().next().is_some();
        declares_items = requires.block.statements().any(|it| matches!(it, ast::Stmt::Item(_)));
        let inner = inner_text(cx, requires.args)?;
        let text = if has_decls { format!("{{ {inner} }}") } else { inner.to_owned() };
        lines.push(("requires", as_clause(cx, &text)?));
    }
    let mut named = None;
    if let Some(ensures) = &ensures {
        if declares_items {
            return Err("its `requires` declares an item, which its `ensures` can name and a native `ensures` can't see".into());
        }
        if ensures.block.statements().next().is_some() {
            return Err("its `ensures` has statements before its condition".into());
        }
        let tail = ensures.block.tail_expr().ok_or("its `ensures` has no condition")?;
        let value = return_value(cx, function, source, &tail)?;
        let reads = match &value {
            Value::Unit => "&()".to_owned(),
            Value::Named(name) => format!("&{name}"),
            Value::Fresh(name, _) => format!("&{name}"),
        };
        let text = match tail {
            ast::Expr::ClosureExpr(closure) => {
                check_captures(cx, function, closure)?;
                let call_form = closure.for_binder().is_some()
                    || closure.ret_type().is_some()
                    || escapes(
                        cx,
                        closure.body().ok_or("its `ensures` closure has no body")?.syntax(),
                        0,
                    )?;
                let list = closure.param_list().ok_or("its `ensures` closure has no parameters")?;
                let param =
                    list.params().next().ok_or("its `ensures` closure takes no parameter")?;
                if call_form {
                    format!("({})({reads})", written_text(cx, closure.syntax())?)
                } else {
                    let pat = written_text(
                        cx,
                        param
                            .pat()
                            .ok_or("its `ensures` closure's parameter has no pattern")?
                            .syntax(),
                    )?;
                    let ty = match param.ty() {
                        Some(ty) => format!(": {}", written_text(cx, ty.syntax())?),
                        None => String::new(),
                    };
                    let body = written_text(
                        cx,
                        closure.body().ok_or("its `ensures` closure has no body")?.syntax(),
                    )?;
                    format!("{{ let {pat}{ty} = {reads}; {body} }}")
                }
            }
            ast::Expr::PathExpr(path) => {
                let resolved = path.path().and_then(|it| cx.path(it));
                if !matches!(
                    resolved,
                    Some(hir::PathResolution::Def(
                        hir::ModuleDef::Function(_) | hir::ModuleDef::Const(_)
                    ))
                ) {
                    return Err("its `ensures` names neither a function nor a `const`, so read at the function's end it may not be what it was at its start".into());
                }
                format!("{}({reads})", written_text(cx, path.syntax())?)
            }
            _ => {
                return Err("its `ensures` is neither a closure nor a path to a function, and a native `ensures` would evaluate it at the function's end, not its start".into());
            }
        };
        lines.push(("ensures", as_clause(cx, &text)?));
        named = match value {
            Value::Fresh(name, ty) => Some((name, ty)),
            Value::Unit | Value::Named(_) => None,
        };
    }

    // The attributes go, with the space after each.
    let text = cx.text();
    for written in requires.iter().chain(ensures.iter()) {
        let end = usize::from(written.attr.end());
        let space = text[end..].len() - text[end..].trim_start().len();
        parts.push((
            TextRange::new(written.attr.start(), written.attr.end() + TextSize::new(space as u32)),
            String::new(),
        ));
    }
    // The clauses go between the signature and the body, each on its own lines.
    let body_at =
        cx.range(source_body.syntax()).ok_or("its body is not where it is written")?.start();
    let before = &text[..usize::from(body_at)];
    let gap_at = TextSize::new(before.trim_end().len() as u32);
    let fn_at = usize::from(source.fn_token().ok_or("it has no `fn`")?.text_range().start());
    let line = &text[text[..fn_at].rfind('\n').map_or(0, |it| it + 1)..];
    let indent = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
    let mut clauses_text = String::new();
    if let Some((name, ty)) = &named {
        parts.push((TextRange::empty(ty.start()), format!("({name}: ")));
        if ty.end() == gap_at {
            clauses_text.push(')');
        } else {
            parts.push((TextRange::empty(ty.end()), ")".to_owned()));
        }
    }
    for (keyword, expr) in &lines {
        clauses_text.push_str(&format!("\n{indent}    {keyword}\n{indent}        {expr},"));
    }
    clauses_text.push_str(&format!("\n{indent}"));
    parts.push((TextRange::new(gap_at, body_at), clauses_text));
    parts.sort_by_key(|(range, _)| (range.start(), range.end()));
    Ok(parts)
}

/// What the `ensures` reads the returned value as.
enum Value {
    /// The function has no `->`: its value is `()`.
    Unit,
    /// The return is named already.
    Named(String),
    /// The return is to be named, its type written at this range.
    Fresh(String, TextRange),
}

/// The name the `ensures` reads the value returned by, chosen as the module docs say.
fn return_value<'t>(
    cx: &Cx<'_, 't>,
    function: ast::Fn<'t>,
    source: ast::Fn<'t>,
    tail: &ast::Expr<'t>,
) -> Result<Value, String> {
    let Some(ret) = source.ret_type() else { return Ok(Value::Unit) };
    if let Some(pat) = ret.pat() {
        let ast::Pat::IdentPat(pat) = pat else {
            return Err("its named return is a pattern, not a name".into());
        };
        let name =
            pat.name().filter(|_| plain(pat)).ok_or("its named return is a pattern, not a name")?;
        return Ok(Value::Named(name.text().to_owned()));
    }
    let ty = ret.ty().ok_or("its return type is missing")?;
    if ty.syntax().descendants().any(|it| it.kind() == SyntaxKind::IMPL_TRAIT_TYPE) {
        return Err("its return type has an `impl Trait`, which a named return can't hold".into());
    }
    let ty_at = cx.range(ty.syntax()).ok_or("its return type is not where it is written")?;
    // The names the function's parameters bind: a return named as one would hide it.
    let mut params = Vec::new(); // alloc: the parameters' names, once per finding
    if let Some(list) = function.param_list() {
        params.push("self");
        for node in list.syntax().descendants() {
            if let Some(name) = ast::IdentPat::cast(node).and_then(|it| it.name()) {
                params.push(name.text());
            }
        }
    }
    if let ast::Expr::ClosureExpr(closure) = tail
        && let Some(param) = closure.param_list().and_then(|it| it.params().next())
        && let Some(ast::Pat::IdentPat(pat)) = param.pat()
        && plain(pat)
        && let Some(name) = pat.name()
        && !params.contains(&name.text())
    {
        return Ok(Value::Fresh(name.text().to_owned(), ty_at));
    }
    // A name no identifier of the function, as written or as expanded, is.
    let mut used = Vec::new(); // alloc: the identifiers of one function, once per finding
    for node in [source.syntax(), function.syntax()] {
        for element in node.descendants_with_tokens() {
            if let NodeOrToken::Token(token) = element
                && token.kind() == SyntaxKind::IDENT
            {
                used.push(token.text());
            }
        }
    }
    let fixed = ["ret", "result", "ret_"].into_iter().map(str::to_owned);
    let numbered = (0..FRESH).map(|it| format!("ret{it}"));
    fixed
        .chain(numbered)
        .find(|it| !used.contains(&it.as_str()))
        .map(|name| Value::Fresh(name, ty_at))
        .ok_or_else(|| {
            format!(
                "each of `ret`, `result`, `ret_` and `ret0` .. `ret{}` is used in it",
                FRESH - 1
            )
        })
}

/// Whether `pat` binds only its name: no `ref`, `mut` or `@`.
fn plain(pat: ast::IdentPat<'_>) -> bool {
    pat.ref_token().is_none()
        && pat.mut_token().is_none()
        && pat.at_token().is_none()
        && pat.pat().is_none()
}

/// That the `ensures` closure reads nothing that may differ between the function's start, where
/// nightly builds it, and its end, where a native `ensures` reads it: only parameters (`self`
/// included) that are not `mut` and are `Copy`.
fn check_captures<'t>(
    cx: &Cx<'_, 't>,
    function: ast::Fn<'t>,
    closure: ast::ClosureExpr<'t>,
) -> Result<(), String> {
    let db = cx.db();
    if closure.async_token().is_some()
        || closure.const_token().is_some()
        || closure.gen_token().is_some()
        || closure.static_token().is_some()
    {
        return Err("its `ensures` closure is `async`, `gen`, `const` or `static`".into());
    }
    let ty = cx.ty(ast::Expr::ClosureExpr(closure)).ok_or("its `ensures` closure has no type")?;
    let closure_ty = ty.as_closure().ok_or("its `ensures` is not typed as a closure")?;
    let mut params = Vec::new(); // alloc: the parameters' bindings, once per finding
    if let Some(list) = function.param_list() {
        for node in list.syntax().descendants() {
            if let Some(local) = ast::IdentPat::cast(node).and_then(|it| cx.local(it)) {
                params.push(local);
            }
        }
    }
    for capture in closure_ty.captured_items(db) {
        let local = capture.local();
        let name = local.name(db);
        let name = name.as_str();
        if !local.is_self(db) && !params.contains(&local) {
            return Err(format!(
                "its `ensures` reads `{name}`, which its `requires` declares and a native `ensures` can't see"
            ));
        }
        if local.is_mut(db) {
            return Err(format!(
                "its `ensures` reads the `mut` parameter `{name}`, which may change before the end, where a native `ensures` reads it"
            ));
        }
        if !local.ty(db).is_copy(db) {
            return Err(format!(
                "its `ensures` reads the parameter `{name}`, which is not `Copy` and may be moved before the end, where a native `ensures` reads it"
            ));
        }
    }
    Ok(())
}

/// Whether `node` has a `return`, `?`, `do yeet` or `become` that leaves the code it is in: not
/// one inside a closure, an `async`, `const` or `gen` block or an item, and through the macros
/// it calls. An error when a macro call can't be expanded or nests past [`MACRO_DEPTH`].
fn escapes<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>, depth: u32) -> Result<bool, String> {
    let mut events = node.preorder();
    while let Some(event) = events.next() {
        let WalkEvent::Enter(it) = event else { continue };
        if let Some(call) = ast::MacroCall::cast(it) {
            if depth >= MACRO_DEPTH {
                return Err(format!("its clauses call macros more than {MACRO_DEPTH} deep"));
            }
            let Some(expansion) = cx.expand(call) else {
                return Err(format!(
                    "the macro call `{}` in a clause can't be expanded",
                    call.syntax().text()
                ));
            };
            if escapes(cx, expansion, depth + 1)? {
                return Ok(true);
            }
            events.skip_subtree();
            continue;
        }
        let nested = it != node
            && (ast::ClosureExpr::cast(it).is_some()
                || ast::Item::cast(it).is_some()
                || ast::BlockExpr::cast(it).is_some_and(|block| {
                    block.async_token().is_some()
                        || block.const_token().is_some()
                        || block.gen_token().is_some()
                }));
        if nested {
            events.skip_subtree();
            continue;
        }
        if matches!(
            it.kind(),
            SyntaxKind::RETURN_EXPR
                | SyntaxKind::TRY_EXPR
                | SyntaxKind::YEET_EXPR
                | SyntaxKind::BECOME_EXPR
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The source of `node` of the expansion as written in the file.
fn written_text<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Result<&'t str, String> {
    let range = cx
        .range(node)
        .ok_or_else(|| format!("`{}` in a clause is made by a macro", node.text()))?;
    Ok(cx.snippet(range))
}

/// What is inside the attribute's `(..)`, trimmed.
fn inner_text<'q>(cx: &Cx<'q, '_>, args: TextRange) -> Result<&'q str, String> {
    let text = cx.snippet(args);
    let inner = text
        .strip_prefix('(')
        .and_then(|it| it.strip_suffix(')'))
        .ok_or("its attribute's arguments are not in `(..)`")?;
    Ok(inner.trim())
}

/// `text` as one expression of a clause list: as it is when the clause grammar reads it so,
/// parenthesized when not (a block would end the list, and a struct literal or a keyword at
/// the head would be read otherwise).
fn as_clause(cx: &Cx<'_, '_>, text: &str) -> Result<String, String> {
    let fits = |text: &str| {
        let probe = format!("fn f() requires {text}, {{}}");
        let parse = syntax::SourceFile::parse(&probe, cx.edition());
        let one =
            parse.root().descendants().find_map(ast::SpecClause::cast).is_some_and(|clause| {
                let mut exprs = clause.exprs();
                exprs.next().is_some_and(|it| it.syntax().text() == text) && exprs.next().is_none()
            });
        one && parse.errors().is_empty()
    };
    if fits(text) {
        return Ok(text.to_owned());
    }
    let wrapped = format!("({text})");
    if fits(&wrapped) {
        return Ok(wrapped);
    }
    Err(format!("`{text}` does not parse as a clause"))
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use base_db::FileId;
    use blaze_hash::HashMap;
    use hir_expand::fixture;

    use crate::{
        check::{Setup, check},
        conf::Conf,
        fix::fix,
        level::CommandLine,
    };

    /// core's contract macros and lang items as part 0's front-end test has them, the macros
    /// re-exported as `core::contracts` names them, and one function with both, the `requires`
    /// imported under another name.
    const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, option, fn, ord, add, builtin_impls
//- /src/lib.rs crate:main
#![feature(contracts, blaze_verify)]
pub mod contracts {
    #[rustc_builtin_macro]
    pub macro contracts_requires($item:item) {}
    #[rustc_builtin_macro]
    pub macro contracts_ensures($item:item) {}
    pub use self::contracts_requires as requires;
    pub use self::contracts_ensures as ensures;
}
#[lang = "contract_check_requires"]
pub fn contract_check_requires<C: Fn() -> bool + Copy>(_: C) {}
#[lang = "contract_build_check_ensures"]
pub fn build_check_ensures<Ret, C: Fn(&Ret) -> bool + Copy + 'static>(c: C) -> C { c }
#[lang = "contract_check_ensures"]
pub fn contract_check_ensures<C: Fn(&Ret) -> bool + Copy, Ret>(_: Option<C>, r: Ret) -> Ret { r }

use contracts::{ensures, requires as pre};

#[pre(let lim = 10; x < lim)]
#[ensures(move |r| *r > x)]
pub fn next(x: u32) -> u32 {
    x + 1
}
"#;

    /// `blaze check --fix` turns `#[requires]` (imported as `pre`) and `#[ensures]` into native
    /// clauses that say the same: the declarations kept in the requires' block, the return
    /// named by the closure's parameter and bound to it by reference as the closure took it,
    /// and the crate still building, with nothing left for the lint.
    #[test]
    fn native_contracts_fix_rewrites_attributes_as_clauses() {
        let fx = fixture::db(FIXTURE);
        let file = *fx.files().last().expect("src/lib.rs");
        let mut db = fx.into_db();
        let krate = blaze_query::db::attach(&db, || crate::tests::main_crate(&db));
        let file = file.file_id(&db);
        let before = db.file_text(file).text(&db).to_string();
        let mut names: HashMap<FileId, String, Global> = HashMap::new_in(Global);
        names.insert(file, "src/lib.rs".to_owned());
        let name_of = |file: FileId| names.get(&file).cloned();
        let flags: Vec<String> =
            ["-A", "clippy::all", "--allow=blaze::general", "--warn=blaze::native_contracts"]
                .into_iter()
                .map(str::to_owned)
                .collect();
        let setup = Setup {
            command_line: CommandLine::parse(&flags).expect("the flags read"),
            conf: Conf::default(),
        };
        let made = fix(&mut db, krate, &setup, &name_of).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(made.applied, 1, "one fix, of `next`");
        let after = db.file_text(file).text(&db).to_string();
        let source = |text: &str| {
            text.split_once("use contracts::").map(|(_, it)| it.to_owned()).unwrap_or_default()
        };
        assert_eq!(
            source(&after),
            "{ensures, requires as pre};

pub fn next(x: u32) -> (r: u32)
    requires
        ({ let lim = 10; x < lim }),
    ensures
        ({ let r = &r; *r > x }),
{
    x + 1
}
",
            "from:\n{before}"
        );
        blaze_query::db::attach(&db, || {
            let found = check(&db, krate, &setup, &name_of).expect("the file is named");
            let left: Vec<&str> = found.iter().map(|it| it.diagnostic.message.as_str()).collect();
            assert!(left.is_empty(), "the fixed file lints clean:\n{after}\n{left:#?}");
        });
    }
}
