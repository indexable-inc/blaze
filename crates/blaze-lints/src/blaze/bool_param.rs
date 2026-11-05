//! `bool_param`: a parameter of type `bool` of a function or method of the workspace, whatever
//! its visibility. A caller's `true` says nothing of what it chooses, two of them swap without
//! a type error, and a third answer has nowhere to go; a two-variant enum named for the choice
//! says it at every call, and a variant added later makes every `match` on it decide.
//!
//! Not checked: a method of a trait impl (its signature is the trait's, found at the trait's
//! declaration when the trait is the workspace's), a function with an ABI (`extern "C" fn`,
//! whose signature is the foreign side's), a closure (not an `fn` item), a setter `set_*`
//! whose one parameter is the `bool` it sets, code under `#[cfg(test)]` or `#[test]`, and a
//! signature a macro wrote. The type is resolved, so an alias of `bool` counts; `&bool` and
//! `Option<bool>` do not. There is no fix: the enum's name and variants are the author's.

use hir::AsAssocItem as _;
use syntax::view::{self as ast, AstNode as _};

use super::Lint;
use crate::check::Cx;

const LINT: Lint = Lint::BoolParam;

/// The prefixes a flag's name often has that its enum's name drops: `is_hot` is `Hot`.
const PREFIXES: &[&str] = &["is_", "has_", "should_", "with_", "use_", "allow_", "enable_", "no_"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    if cx.expansion().is_some() || !cx.is_written(f.syntax()) || f.abi().is_some() {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    if func.as_assoc_item(db).is_some_and(|it| it.implemented_trait(db).is_some()) {
        return;
    }
    if cx.is_in_test(f.syntax()) {
        return;
    }
    let Some(list) = f.param_list() else { return };
    let params = func.params_without_self(db);
    let name = func.name(db);
    let name = name.as_str();
    if name.starts_with("set_") && params.len() == 1 && params[0].ty().is_bool() {
        return;
    }
    for (param, hir_param) in list.params().zip(params.iter()) {
        if !hir_param.ty().is_bool() {
            continue;
        }
        let Some(ty) = param.ty() else { continue };
        let Some(range) = cx.range(ty.syntax()) else { continue };
        let pat = param.pat().map(|it| cx.source_of(it.syntax()).into_owned());
        let choice = choice_name(pat.as_deref().unwrap_or_default());
        let message = match &pat {
            Some(pat) => format!(
                "`fn {name}` takes `{pat}: bool`: a caller's `true` or `false` does not say what it chooses"
            ),
            None => format!(
                "`fn {name}` takes a `bool`: a caller's `true` or `false` does not say what it chooses"
            ),
        };
        let span = cx.span(range);
        cx.blaze(LINT, ty.syntax(), span, message)
            .note(format!(
                "take `{choice}`, an `enum {choice} {{ .. }}` of two variants each named for what it does: \
                 the call says the choice, and a third answer is a variant every `match` must decide"
            ))
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The name of the enum a flag named `pat` becomes: its words in upper camel case, a prefix of
/// [`PREFIXES`] dropped; `Choice` when it has none.
fn choice_name(pat: &str) -> String {
    let bare = pat.trim_start_matches("mut ").trim_start_matches("r#").trim_start_matches('_');
    let bare = PREFIXES.iter().find_map(|it| bare.strip_prefix(it)).unwrap_or(bare);
    // alloc: the enum's name, once per finding
    let mut out = String::with_capacity(bare.len());
    for word in bare.split('_').filter(|it| !it.is_empty()) {
        if !word.chars().all(|it| it.is_alphanumeric()) {
            return "Choice".to_owned(); // alloc: the enum's name, once per finding
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() || out.starts_with(|it: char| it.is_ascii_digit()) {
        return "Choice".to_owned(); // alloc: the enum's name, once per finding
    }
    out
}

#[cfg(test)]
mod tests {
    use super::choice_name;

    #[test]
    fn a_flag_is_named_for_its_choice() {
        assert_eq!(choice_name("verbose"), "Verbose");
        assert_eq!(choice_name("is_hot"), "Hot");
        assert_eq!(choice_name("mut keep_going"), "KeepGoing");
        assert_eq!(choice_name("_dry_run"), "DryRun");
        assert_eq!(choice_name("_"), "Choice");
        assert_eq!(choice_name("(a, b)"), "Choice");
    }
}
