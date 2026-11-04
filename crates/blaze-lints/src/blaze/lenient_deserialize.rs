//! `lenient_deserialize` (in `blaze::internal`, allowed until a workspace turns it on; blaze's
//! own workspace does): a struct or enum that derives serde's `Deserialize` without
//! `#[serde(deny_unknown_fields)]`. serde's derive then skips a key it has no field for, so a
//! misspelled or renamed field in a config, a manifest or a message reads as absent and its value
//! is silently dropped: a fallback on input (AGENTS.md rule 3, no-fallback).
//!
//! Found: a derive path that name resolution takes to the `Deserialize` derive of `serde_derive`,
//! `serde` or `blaze_serde_derive` (bare, `serde::Deserialize`, `blaze_serde::Deserialize`,
//! through `use`, renames and re-exports), on an item of the file whose own `#[serde(..)]`
//! attributes (those inside a `cfg_attr` included) say no `deny_unknown_fields`. Where the
//! attribute means nothing, or serde refuses it, nothing is found:
//!
//! - a tuple or unit struct: it is read as a sequence or a unit, which has no field names;
//! - a struct with a `#[serde(flatten)]` field: serde (and blaze-serde-derive) refuse
//!   `deny_unknown_fields` beside `flatten`, since the flattened field takes the keys left over;
//! - `#[serde(transparent)]`, `#[serde(from = "..")]`, `#[serde(try_from = "..")]`: the input is
//!   read as another type, whose own derive is where unknown fields are refused;
//! - an enum with no struct variant (`V { .. }`): its unit, newtype and tuple variants have no
//!   field names, and an unknown variant name is already an error. On an enum with one, the
//!   attribute refuses unknown fields in every struct variant (externally, internally and
//!   adjacently tagged alike, as serde reads it), so one finding is on the enum; an enum with a
//!   `#[serde(flatten)]` field in any variant is left alone, as a struct with one is;
//! - an `#[serde(untagged)]` enum: blaze-serde-derive tries each variant with unknown fields
//!   allowed whatever the container says, so the attribute would promise what it doesn't do.
//!
//! Not checked: test code, an item a macro wrote (its derive is the macro's), an item declared in
//! a function body (its derive resolves in the body's tree, which this walk doesn't ask), and a
//! derive inside a `cfg_attr`.
//!
//! The fix, machine-applicable, writes `#[serde(deny_unknown_fields)]` on the line after the
//! derive, at its indentation. A type that must read input from a newer writer (a format that
//! grows on purpose) keeps its leniency with `#[expect(blaze::lenient_deserialize, reason =
//! "..")]` on the item.

use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::{Lint, facts};
use crate::{
    check::{self, Cx},
    diag::Applicability,
};

const LINT: Lint = Lint::LenientDeserialize;

/// The crates whose `Deserialize` derive this lint knows: serde's, through `serde` or its
/// `serde_derive`, and blaze's (`blaze_serde`'s re-export of `blaze_serde_derive`).
const DERIVE_CRATES: &[&str] = &["serde_derive", "serde", "blaze_serde_derive"];

const ATTRIBUTE: &str = "#[serde(deny_unknown_fields)]";

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, adt: ast::Adt<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.in_file()
        || !cx.is_written(adt.syntax())
        || cx.is_in_test(adt.syntax())
    {
        return;
    }
    // The syntax first: name resolution only for an item the attribute would change.
    if says(adt.attrs(), "deny_unknown_fields") || !meaningful(adt) {
        return;
    }
    let Some(derive) = deserialize_derive(cx, adt) else { return };
    let Some(name) = adt.name() else { return };
    let kind = match adt {
        ast::Adt::Enum(_) => "enum",
        ast::Adt::Struct(_) => "struct",
        ast::Adt::Union(_) => "union",
    };
    let Some(attr_range) = cx.range(derive.attr.syntax()) else { return };
    let message = format!(
        "{kind} `{}` derives `Deserialize` without `#[serde(deny_unknown_fields)]`: an unknown field in its input is silently dropped",
        name.text()
    );
    // alloc: a message's part
    let note = "a misspelled or renamed field reads as absent, and what it said is lost without an error; \
                `#[serde(deny_unknown_fields)]` makes it an error naming the field"
        .to_owned();
    let text = cx.text();
    let start = usize::from(attr_range.start()).min(text.len());
    let line = &text[text[..start].rfind('\n').map_or(0, |it| it + 1)..start];
    let indent = &line[..line.len() - line.trim_start().len()];
    let insert = cx.span(TextRange::empty(attr_range.end()));
    let fix = format!("\n{indent}{ATTRIBUTE}"); // alloc: the fix's text
    let span = cx.span(derive.path);
    let diagnostic = cx.blaze(LINT, adt.syntax(), span, message);
    diagnostic.note(note);
    diagnostic.multipart_suggestion(
        // alloc: the suggestion's label
        format!("refuse unknown fields with `{ATTRIBUTE}`"),
        // alloc: the fix's one part
        vec![(insert, fix)],
        Applicability::MachineApplicable,
    );
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// The `#[derive(..)]` attribute naming serde's `Deserialize`, and where the path is written.
struct Derive<'t> {
    attr: ast::Attr<'t>,
    path: TextRange,
}

/// The derive of `adt` that name resolution takes to a `Deserialize` derive of
/// [`DERIVE_CRATES`].
fn deserialize_derive<'t>(cx: &Cx<'_, 't>, adt: ast::Adt<'t>) -> Option<Derive<'t>> {
    let db = cx.db();
    adt.attrs().filter(|it| it.simple_name() == Some("derive")).find_map(|attr| {
        let macros = cx.derive_macros(attr);
        check::derive_paths(attr).into_iter().zip(macros).find_map(|((path, _), found)| {
            let found = found?;
            let krate = found.module(db).krate(db);
            (found.name(db).as_str() == "Deserialize"
                && DERIVE_CRATES.iter().any(|it| facts::crate_is(db, krate, it)))
            .then_some(Derive { attr, path })
        })
    })
}

/// Whether `#[serde(deny_unknown_fields)]` on `adt` would refuse something and serde accepts it
/// there (the module's list of exemptions).
fn meaningful(adt: ast::Adt<'_>) -> bool {
    if ["transparent", "from", "try_from", "untagged"].iter().any(|it| says(adt.attrs(), it)) {
        return false;
    }
    match adt {
        ast::Adt::Struct(it) => match it.field_list() {
            Some(ast::FieldList::RecordFieldList(fields)) => !flattens(fields),
            Some(ast::FieldList::TupleFieldList(_)) | None => false,
        },
        ast::Adt::Enum(it) => {
            let mut named = false;
            for variant in it.variant_list().into_iter().flat_map(|it| it.variants()) {
                if let Some(ast::FieldList::RecordFieldList(fields)) = variant.field_list() {
                    named = true;
                    if flattens(fields) {
                        return false;
                    }
                }
            }
            named
        }
        ast::Adt::Union(_) => false,
    }
}

/// Whether a field of `fields` is `#[serde(flatten)]`.
fn flattens(fields: ast::RecordFieldList<'_>) -> bool {
    fields.fields().any(|it| says(it.attrs(), "flatten"))
}

/// Whether one of `attrs`, or an attribute a `cfg_attr` of them holds, is `#[serde(..)]` with
/// `word` as one of its items (`deny_unknown_fields`, `rename = ".."`'s `rename`).
fn says<'t>(attrs: impl Iterator<Item = ast::Attr<'t>>, word: &str) -> bool {
    attrs.filter_map(|it| it.meta()).flat_map(|it| it.skip_cfg_attrs()).any(|meta| {
        meta.as_simple_call()
            .is_some_and(|(name, tt)| name == "serde" && words(tt).any(|it| it == word))
    })
}

/// The first token of each comma-separated item of `tt`, `#[serde(..)]`'s list: `rename` of
/// `rename = "x"`, `bound` of `bound(deserialize = "..")`.
fn words<'t>(tt: ast::TokenTree<'t>) -> impl Iterator<Item = &'t str> {
    let mut first = true;
    tt.syntax().children_with_tokens().filter_map(move |child| match child {
        NodeOrToken::Token(token) if token.kind() == SyntaxKind::COMMA => {
            first = true;
            None
        }
        NodeOrToken::Token(token)
            if token.kind().is_trivia()
                || matches!(token.kind(), SyntaxKind::L_PAREN | SyntaxKind::R_PAREN) =>
        {
            None
        }
        NodeOrToken::Token(token) => std::mem::replace(&mut first, false).then(|| token.text()),
        NodeOrToken::Node(_) => {
            first = false;
            None
        }
    })
}
