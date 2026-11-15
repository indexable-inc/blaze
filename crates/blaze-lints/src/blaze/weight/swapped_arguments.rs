//! `swapped_arguments`: two arguments of one type passed in each other's place, which the
//! names say: `copy(dst, src)` into `fn copy(src: &Path, dst: &Path)`. Both compile; the
//! types cannot tell them apart, the names can.
//!
//! Google Error Prone's `ArgumentSelectionDefectChecker` (Rice et al., "Detecting argument
//! selection defects", OOPSLA 2017): compare each argument's name with each parameter's, as
//! the words of their snake or camel case, and find a pair whose swap is a strict improvement
//! for both: each argument's name is more like the other's parameter than its own, and like it
//! by at least half its words. Narrowed as Error Prone narrows it: a name of fewer than three
//! letters, `arg0` and the like, and an argument with no name (a literal, an arithmetic
//! expression) say nothing and are left out; the two parameters and the two arguments must
//! have one type, so the swap still compiles.
//!
//! The fix swaps the two arguments, `MaybeIncorrect`: the names may be the ones that are wrong.

use syntax::view::{self as ast, AstNode as _, HasArgList as _};

use super::{Fix, Lint, report};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::SwappedArguments;

/// How alike two names must be, in percent of their words, for a swap to be said.
const LEAST: u32 = 50;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let db = cx.db();
    let (func, list, by_path) = if let Some(call) = ast::MethodCallExpr::cast(node) {
        (cx.method(call), call.arg_list(), false)
    } else if let Some(call) = ast::CallExpr::cast(node) {
        let callee = match call.expr().map(|it| it.peel_parens()) {
            Some(ast::Expr::PathExpr(path)) => path.path().and_then(|it| cx.path(it)),
            _ => None,
        };
        let func = match callee {
            Some(hir::PathResolution::Def(hir::ModuleDef::Function(it))) => Some(it),
            _ => None,
        };
        (func, call.arg_list(), true)
    } else {
        return;
    };
    let (Some(func), Some(list)) = (func, list) else { return };
    // A call by path passes `self` first, as the method's first parameter.
    let params = if by_path { func.assoc_fn_params(db) } else { func.params_without_self(db) };
    // alloc: a call's few arguments and their words
    let args: Vec<ast::Expr<'t>> = list.args().collect();
    if args.len() != params.len() || args.len() < 2 {
        return;
    }
    let arg_words: Vec<Option<Vec<String>>> =
        args.iter().map(|it| name_of(*it).and_then(|name| words(&name))).collect();
    let param_names: Vec<Option<String>> =
        params.iter().map(|it| it.name(db).map(|name| name.as_str().to_owned())).collect();
    let param_words: Vec<Option<Vec<String>>> =
        param_names.iter().map(|it| it.as_deref().and_then(words)).collect();
    let mut taken = vec![false; args.len()]; // alloc: as `args`
    for i in 0..args.len() {
        for j in i + 1..args.len() {
            if taken[i] || taken[j] {
                continue;
            }
            let (Some(ai), Some(aj), Some(pi), Some(pj)) =
                (&arg_words[i], &arg_words[j], &param_words[i], &param_words[j])
            else {
                continue;
            };
            let (kept_i, kept_j) = (alike(ai, pi), alike(aj, pj));
            let (moved_i, moved_j) = (alike(ai, pj), alike(aj, pi));
            if moved_i <= kept_i || moved_j <= kept_j || moved_i < LEAST || moved_j < LEAST {
                continue;
            }
            if params[i].ty() != params[j].ty() {
                continue;
            }
            match (cx.ty(args[i]), cx.ty(args[j])) {
                (Some(a), Some(b)) if a == b => {}
                _ => continue,
            }
            let (Some(at_i), Some(at_j)) = (cx.range(args[i].syntax()), cx.range(args[j].syntax()))
            else {
                continue;
            };
            taken[i] = true;
            taken[j] = true;
            let (text_i, text_j) = (cx.snippet(at_i).to_owned(), cx.snippet(at_j).to_owned());
            let (name_i, name_j) =
                (param_names[i].as_deref().unwrap_or("_"), param_names[j].as_deref().unwrap_or("_"));
            let message = format!(
                "`{text_i}` is passed as `{name_i}` and `{text_j}` as `{name_j}` of `fn {}`: the names say they are swapped",
                func.name(db).as_str()
            );
            let fix = Fix {
                message: "swap the two arguments",
                // alloc: the fix's two parts
                parts: vec![(at_i, text_j), (at_j, text_i)],
                applicability: Applicability::MaybeIncorrect,
            };
            let range = at_i.cover(at_j);
            report(
                cx,
                LINT,
                node,
                range,
                message,
                "both arguments have the parameters' one type, so the call compiles either way; \
                 a newtype for each (or a struct of named fields) makes the swap a type error",
                Some(fix),
            );
        }
    }
}

/// The name an argument is written with: a local's or a path's last segment, a field's, a
/// getter's (`x.len()`, `get_len()`), through `&`, `*` and parentheses.
fn name_of(expr: ast::Expr<'_>) -> Option<String> {
    match expr.peel_parens() {
        ast::Expr::PathExpr(path) => {
            let segment = path.path()?.segment()?;
            Some(segment.name_ref()?.text().to_owned())
        }
        ast::Expr::FieldExpr(field) => Some(field.name_ref()?.text().to_owned()),
        ast::Expr::MethodCallExpr(call) => {
            if call.arg_list().is_some_and(|it| it.args().next().is_some()) {
                return None;
            }
            let name = call.name_ref()?.text();
            Some(name.strip_prefix("get_").unwrap_or(name).to_owned())
        }
        ast::Expr::RefExpr(it) => name_of(it.expr()?),
        ast::Expr::PrefixExpr(it) => name_of(it.expr()?),
        _ => None,
    }
}

/// The words of a name, lower case: `src_path` and `srcPath` are `src`, `path`. `None` for a
/// name that says nothing: shorter than three letters, or `arg` and a number.
pub(super) fn words(name: &str) -> Option<Vec<String>> {
    let name = name.trim_start_matches("r#").trim_matches('_');
    if name.len() < 3 {
        return None;
    }
    let bare = name.trim_end_matches(|it: char| it.is_ascii_digit());
    if matches!(bare, "arg" | "param" | "val" | "value" | "tmp") {
        return None;
    }
    // alloc: a name's few words
    let mut out = Vec::new();
    let mut word = String::new();
    let mut last_lower = false;
    for ch in name.chars() {
        if ch == '_' || (ch.is_uppercase() && last_lower) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
        }
        last_lower = ch.is_lowercase() || ch.is_ascii_digit();
        if ch != '_' {
            word.extend(ch.to_lowercase());
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out.sort_unstable();
    out.dedup();
    (!out.is_empty()).then_some(out)
}

/// How alike two names' words are, in percent: the words they share over the words of either.
pub(super) fn alike(a: &[String], b: &[String]) -> u32 {
    let shared = a.iter().filter(|it| b.contains(it)).count();
    let either = a.len() + b.len() - shared;
    if either == 0 {
        return 0;
    }
    u32::try_from(shared * 100 / either).unwrap_or(100)
}
