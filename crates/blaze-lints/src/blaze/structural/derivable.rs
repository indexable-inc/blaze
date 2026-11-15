//! `hand_written_derivable`: an impl of `Clone`, `Debug`, `Default`, `PartialEq`, `PartialOrd`,
//! `Ord` or `Hash` for a struct that is token for token what `#[derive(..)]` writes. Written
//! by hand it must be edited with every field the struct gains, and one that is not compiles
//! and is wrong; the derive covers the new field with no edit.
//!
//! What counts as the derive's, by [`super::uniform`]'s template over the struct's fields:
//!
//! - `Clone`: `Self { a: self.a.clone(), .. }` (a `Copy` field copied or cloned alike), or a
//!   tuple struct's `Self(self.0.clone(), ..)`, every field once; or `*self` when the struct
//!   derives `Copy`;
//! - `Debug`: `f.debug_struct("Name").field("a", &self.a)...finish()`, every field once, in
//!   declaration order, each named by its own name;
//! - `Default`: every field given its type's default (`Default::default()`, `T::default()`, `0`,
//!   `false`, `None`, `""`, an empty std collection);
//! - `PartialEq`: `self.a == other.a && ..`, every field once;
//! - `Hash`: `self.a.hash(state); ..`, every field once, in declaration order, when
//!   `PartialEq` is derived too (clippy's `derived_hash_with_manual_eq` refuses a derived
//!   `Hash` beside a hand-written `PartialEq`);
//! - `Ord`: `self.a.cmp(&other.a).then(..)` or `.then_with(|| ..)`, every field once, in
//!   declaration order, when `PartialOrd` is derived too; and `PartialOrd` written
//!   `Some(self.cmp(other))` beside such an `Ord`.
//!
//! Not checked: a generic struct or impl (a derive bounds every type parameter by the trait,
//! which the impl may not), an impl with attributes, an impl with any other item (`clone_from`,
//! `ne`), `Eq` (a derive asserts every field is `Eq`, which an empty `impl Eq` does not), and
//! enums.
//!
//! Fix (machine-applicable, maybe-incorrect when a comment in a removed impl would be lost):
//! the impls go and their traits join the struct's `#[derive(..)]`, or a new one.

use std::alloc::Global;

use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _, HasGenericParams as _, HasVisibility as _},
};

use super::{
    Impl, Items, derived,
    uniform::{self, DEFAULT, Form, Shape},
};
use crate::{
    blaze::{
        Lint,
        std_mistakes::{Fix, report},
    },
    check::Cx,
    diag::Applicability,
};

const LINT: Lint = Lint::HandWrittenDerivable;

/// The traits this lint derives, in the order a derive list writes them.
const ORDER: &[&str] =
    &["Clone", "Copy", "Debug", "Default", "PartialEq", "Eq", "PartialOrd", "Ord", "Hash"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, items: &Items<'t>, impls: &[Impl<'t>]) {
    if !cx.on_blaze(LINT) {
        return;
    }
    for &strukt in &items.structs {
        if strukt.generic_param_list().is_some() {
            continue;
        }
        let Some(adt) = cx.adt_def(ast::Adt::Struct(strukt)) else { continue };
        let derives = derived(strukt);
        let mut found: Vec<(&Impl<'t>, &'static str), Global> = Vec::new_in(Global);
        for imp in impls.iter().filter(|it| it.adt == adt && !it.generic) {
            if let Some(name) = derive_of(imp, &derives) {
                found.push((imp, name));
            }
        }
        // A derived `Hash` needs a derived `PartialEq`, a derived `Ord` a derived `PartialOrd`,
        // and a `PartialOrd` that calls `cmp` a derived `Ord`: drop each whose partner stays
        // hand-written, until none is dropped.
        loop {
            let has = |found: &[(&Impl<'t>, &str)], name: &str| {
                derives.contains(&name) || found.iter().any(|(_, it)| *it == name)
            };
            let before = found.len();
            let mut kept: Vec<(&Impl<'t>, &'static str), Global> = Vec::new_in(Global);
            for &(imp, name) in &found {
                let partner = match name {
                    "Hash" => Some("PartialEq"),
                    "Ord" => Some("PartialOrd"),
                    "PartialOrd" => Some("Ord"),
                    _ => None,
                };
                if partner.is_none_or(|it| has(&found, it)) {
                    kept.push((imp, name));
                }
            }
            found = kept;
            if found.len() == before {
                break;
            }
        }
        if found.is_empty() {
            continue;
        }
        found.sort_by_key(|(_, name)| ORDER.iter().position(|it| it == name));
        let first = found.iter().map(|(imp, _)| *imp).min_by_key(|it| it.node.syntax().text_range().start());
        let Some(first) = first else { continue };
        let mut names: Vec<&str, Global> = Vec::new_in(Global);
        names.extend(found.iter().map(|(_, name)| *name));
        let mut said: Vec<String, Global> = Vec::new_in(Global);
        said.extend(found.iter().map(|(imp, _)| format!("`{}`", imp.said())));
        let message = format!(
            "{} {} what `#[derive({})]` writes: by hand, {} must be edited with every field `{}` \
             gains",
            joined(&said),
            if said.len() == 1 { "is" } else { "are" },
            names.join(", "),
            if said.len() == 1 { "it" } else { "each" },
            first.type_name,
        );
        let fix = fix(cx, strukt, &found, &names);
        let node = first.node.syntax();
        let header = header(first.node);
        report(
            cx,
            LINT,
            node,
            header,
            message,
            "a derive covers a field added later with no edit; the end state is the comptime \
             reflection these derives become library code on (`for const f in fields_of::<T>()`)",
            fix,
        );
    }
}

/// The trait whose derive `imp` is, token for token, before its partner is asked for.
fn derive_of(imp: &Impl<'_>, derives: &[&str]) -> Option<&'static str> {
    let list = imp.node.assoc_item_list()?;
    if !imp.std || imp.node.attrs().next().is_some() || list.assoc_items().count() != 1 {
        return None;
    }
    let [method] = imp.methods.as_slice() else { return None };
    let n = imp.fields.len();
    let tuple = imp.fields.first().is_some_and(|it| it == "0");
    let analysis = &method.analysis;
    let covers = analysis.shape == Shape::Uniform && analysis.covered.len() == n && n > 0;
    let block = method.node.body()?;
    let other = method.params.first().copied().unwrap_or_default();
    match (imp.trait_name?, method.name) {
        ("Clone", "clone") => {
            let copied = uniform::tokens(block.syntax()) == "{ * self }" && derives.contains(&"Copy");
            let field_wise = matches!(method.body.form, Form::Record | Form::Tuple)
                && covers
                && analysis.template == "self . #";
            (copied || field_wise).then_some("Clone")
        }
        ("Debug", "fmt") if !tuple => {
            let Form::Chain { head, link: "field", after } = &method.body.form else { return None };
            let named = uniform::tokens(head.syntax())
                == format!("{other} . debug_struct ( \"{}\" )", imp.type_name);
            let ends = after.as_slice() == ["finish"];
            let template = analysis.template == "( \"#\" , & self . # )";
            (named && ends && template && covers && analysis.in_order(n)).then_some("Debug")
        }
        ("Default", "default") => {
            let built = matches!(method.body.form, Form::Record | Form::Tuple);
            (built && covers && analysis.template == DEFAULT).then_some("Default")
        }
        ("PartialEq", "eq") => {
            let joined = matches!(
                method.body.form,
                Form::Operator(ast::BinaryOp::LogicOp(ast::LogicOp::And)) | Form::One
            );
            let template = analysis.template == format!("self . # == {other} . #");
            (joined && template && covers).then_some("PartialEq")
        }
        ("Hash", "hash") => {
            let statements = matches!(method.body.form, Form::Statements | Form::One);
            let template = analysis.template == format!("self . # . hash ( {other} )");
            let framed = analysis.frame == 0;
            (statements && template && framed && covers && analysis.in_order(n)).then_some("Hash")
        }
        ("Ord", "cmp") => {
            let chained = match &method.body.form {
                Form::Chain { link, after, .. } => {
                    matches!(*link, "then" | "then_with") && after.is_empty()
                }
                Form::One => true,
                _ => false,
            };
            let template = analysis.template == format!("self . # . cmp ( & {other} . # )");
            (chained && template && covers && analysis.in_order(n)).then_some("Ord")
        }
        ("PartialOrd", "partial_cmp") => {
            let text = uniform::tokens(block.syntax());
            let canonical = text == format!("{{ Some ( self . cmp ( {other} ) ) }}")
                || text == format!("{{ Some ( Ord :: cmp ( self , {other} ) ) }}");
            canonical.then_some("PartialOrd")
        }
        _ => None,
    }
}

/// `impl Clone for Point`: the impl up to its `{`.
fn header(imp: ast::Impl<'_>) -> TextRange {
    let range = imp.syntax().text_range();
    match imp.assoc_item_list() {
        Some(list) => TextRange::new(range.start(), list.syntax().text_range().start()),
        None => range,
    }
}

/// The impls removed and their traits added to the struct's derive.
fn fix<'t>(
    cx: &Cx<'_, 't>,
    strukt: ast::Struct<'t>,
    found: &[(&Impl<'t>, &str)],
    names: &[&str],
) -> Option<Fix> {
    let text = cx.text();
    let mut parts: Vec<(TextRange, String), Global> = Vec::new_in(Global);
    let mut applicability = Applicability::MachineApplicable;
    for (imp, _) in found {
        let node = imp.node.syntax();
        let commented = node.descendants_with_tokens().any(|it| match it {
            NodeOrToken::Token(token) => token.kind() == SyntaxKind::COMMENT,
            NodeOrToken::Node(_) => false,
        });
        if commented {
            applicability = Applicability::MaybeIncorrect;
        }
        parts.push((lines_of(text, node.text_range())?, String::new()));
    }
    let list = names.join(", ");
    let existing = strukt.attrs().find(|it| it.simple_name() == Some("derive"));
    match existing {
        Some(attr) => {
            let tree = attr.meta()?.syntax().children().find_map(ast::TokenTree::cast)?;
            let close = tree.r_paren_token()?;
            let empty = !tree
                .syntax()
                .children_with_tokens()
                .filter_map(|it| it.into_token())
                .any(|it| it.kind() == SyntaxKind::IDENT);
            let at = TextRange::empty(close.text_range().start());
            parts.push((at, if empty { list } else { format!(", {list}") }));
        }
        None => {
            let start = match strukt.visibility() {
                Some(it) => it.syntax().text_range().start(),
                None => strukt.struct_token()?.text_range().start(),
            };
            let at = usize::from(start);
            let line = text[..at].rfind('\n').map_or(0, |it| it + 1);
            let indent = &text[line..at];
            let indent = if indent.trim().is_empty() { indent } else { "" };
            parts.push((TextRange::empty(start), format!("#[derive({list})]\n{indent}")));
        }
    }
    parts.sort_by_key(|(range, _)| range.start());
    Some(Fix { message: "derive the traits and remove the impls", parts, applicability })
}

/// `range` grown to whole lines when nothing else is on them: the text a removed item leaves
/// no blank line for.
fn lines_of(text: &str, range: TextRange) -> Option<TextRange> {
    let (start, end) = (usize::from(range.start()), usize::from(range.end()));
    let line = text.get(..start)?.rfind('\n').map_or(0, |it| it + 1);
    let start = if text[line..start].trim().is_empty() { line } else { start };
    let rest = text.get(end..)?;
    let end = match rest.find('\n') {
        Some(at) if rest[..at].trim().is_empty() => end + at + 1,
        _ => end,
    };
    Some(TextRange::new(
        TextSize::new(u32::try_from(start).ok()?),
        TextSize::new(u32::try_from(end).ok()?),
    ))
}

/// "`a`, `b` and `c`".
fn joined(said: &[String]) -> String {
    match said {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., tail] => format!("{} and {tail}", init.join(", ")),
    }
}
