//! `proc_macro_derive`, `proc_macro_attribute`, `proc_macro_fn_like` (the group
//! `blaze::proc_macro`): a procedural macro a crate of the workspace defines, one finding per
//! macro, at the name of the function that is the macro, naming the macro and its kind.
//!
//! A procedural macro is opaque code that runs at build time and rewrites tokens: what it
//! generates is not checked until it is expanded, cannot be analysed or cached per item, and
//! reruns whenever the tokens of its input move. What replaces it differs by kind, so each kind
//! is its own lint with its own default level ([`Lint::warns_by_default`]) and its own help
//! ([`Lint::help`]): a derive and an attribute macro wait for compile-time reflection
//! (docs/v2/plan/design/comptime.md, phase 2), a function-like macro that builds data is a
//! `const fn` today, and one that parses a foreign syntax may stay.
//!
//! Found by what name resolution made of the function, never by its attribute's text: the
//! crate is a proc-macro crate (`proc-macro = true`: [`base_db::TargetKind`]) and its def map
//! has the function as a macro (`hir::Function::as_proc_macro`), which says its kind and, for
//! a derive, the name it is derived by. So a `#[proc_macro]` in a crate that is no proc-macro
//! crate (an error of the compile) is not a finding, and a `cfg_attr` that adds the attribute
//! is one.
//!
//! Only definitions are found. A use of another crate's macro (`#[derive(Serialize)]`) is
//! not: a workspace cannot rewrite serde. A dependency's proc-macro crate is not linted at
//! all (`blaze check` lints the workspace's packages; a registry or git crate's lints are
//! capped at `allow`). No fix: the rewrite is the author's.

use hir_expand::MacroKind;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::Lint;
use crate::check::Cx;

/// What every finding says of procedural macros, whatever the kind.
const OPAQUE: &str = "a procedural macro is opaque code that runs at build time and rewrites \
                      tokens: what it generates is not checked until it is expanded, cannot be \
                      analysed or cached per item, and reruns whenever the tokens of its input \
                      move";

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    let db = cx.db();
    if !cx.krate().target_kind(db).is_proc_macro() {
        return;
    }
    let Some(mac) = cx.fn_def(f).and_then(|it| it.as_proc_macro(db)) else { return };
    let (lint, kind) = match mac.kind(db) {
        MacroKind::Derive => (Lint::ProcMacroDerive, "derive macro"),
        MacroKind::Attr => (Lint::ProcMacroAttribute, "attribute macro"),
        MacroKind::ProcMacro => (Lint::ProcMacroFnLike, "function-like macro"),
        // No function of a crate is one of these.
        MacroKind::Declarative
        | MacroKind::DeclarativeBuiltIn
        | MacroKind::DeriveBuiltIn
        | MacroKind::AttrBuiltIn => return,
    };
    if !cx.on_blaze(lint) {
        return;
    }
    let Some(name) = f.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let message = message(kind, mac.name(db).as_str());
    let span = cx.span(range);
    cx.blaze(lint, f.syntax(), span, message)
        .note(OPAQUE)
        .help(lint.help())
        .help(format!("for further information visit {}", lint.docs_url()));
}

/// The finding's first line: the macro by the name it is called by, and its kind.
fn message(kind: &str, name: &str) -> String {
    format!("the {kind} `{name}` is a procedural macro, which blaze cannot see through")
}

#[cfg(test)]
mod tests {
    use super::message;

    #[test]
    fn a_finding_names_the_macro_and_its_kind() {
        assert_eq!(
            message("derive macro", "Serialize"),
            "the derive macro `Serialize` is a procedural macro, which blaze cannot see through"
        );
    }
}
