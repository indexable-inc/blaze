//! The crate attributes given on the command line (`-Z crate-attr`) that rustc refuses, as it
//! reports them. rustc lexes and parses each on its own once the crate root is parsed, before
//! expansion reads any module's file (`rustc_interface/src/passes.rs` `parse`,
//! `rustc_builtin_macros/src/cmdline_attrs.rs` `inject`), as the source `#![<attr>]`: its
//! spans, columns and source lines are that text's. Each attribute is a file of its own with that
//! text (base-db's `crate_attrs`), so its lexer's, delimiters' and parser's errors are any file's
//! ([`lower_file`]); an attribute that `parse_attribute` reads whole, closed by a `]` written in
//! it, is `parse_in`'s `unexpected` at the token after it. An attribute refused is not applied
//! and the crate goes on being checked: only an error rustc's lexer raises as fatal (an
//! unterminated literal or comment) stops it.

use base_db::{CrateAttrError, EditionedFileId};
use blaze_items::Fact;
use hir::HirFileId;
use syntax::{
    LexedStr, SyntaxError, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use super::{File, Outcome, is_multiline, lower_file, tokens};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

/// The errors of each of `cx.krate`'s crate attributes rustc refuses, in the order given, and
/// whether rustc stops there. Each read the crate's attributes, which are its data's.
pub(super) fn lower(cx: &Cx<'_, '_>, found: &mut Vec<Found>) -> bool {
    let db = cx.db;
    let krate = cx.krate.base();
    for &file in base_db::crate_attr_files(db, krate) {
        let Some(error) = base_db::crate_attr_error(db, file) else { continue };
        let before = found.len();
        let fatal = refused(cx, file, error, found);
        let fact = Fact::Crate(blaze_items::key::crate_key(db, krate));
        for it in &mut found[before..] {
            // alloc: the fact's words, for a crate attribute rustc refuses
            it.diag.fact(fact, "the attributes given by `-Z crate-attr`");
        }
        if fatal {
            return true;
        }
    }
    false
}

/// The errors of the crate attribute of `file`, which rustc refuses for `error`, and whether
/// rustc stops there.
fn refused(cx: &Cx<'_, '_>, file: EditionedFileId, error: CrateAttrError, found: &mut Vec<Found>) -> bool {
    let db = cx.db;
    let (file_id, edition) = file.unpack(db);
    let text = cx.file_text(file_id);
    let id = HirFileId::from(file);
    let tree = cx.root(id);
    let errors = file.parse_errors_with_bodies(db).unwrap_or_default();
    if let CrateAttrError::UnexpectedToken { .. } = error {
        let lexed = LexedStr::new(edition, text);
        let f = File { cx, file: id, text, edition, tree, toks: tokens(&lexed, text) };
        if let Some(diag) = f.unexpected_after_attr(errors) {
            found.push(Found::new(diag, Phase::Parse));
            return false;
        }
    }
    let before = found.len();
    let outcome = lower_file(cx, id, text, edition, tree, errors, found);
    if found.len() == before {
        let whole = TextRange::up_to(TextSize::of(text));
        // alloc: the error's text, of a crate attribute blaze can't say why rustc refuses
        let message = format!("blaze can't say why rustc refuses this crate attribute ({error:?})");
        found.push(Found::new(Diag::error(None, message, cx.span_in_file(file_id, whole)), Phase::Parse));
    }
    matches!(outcome, Outcome::Fatal)
}

impl File<'_, '_, '_> {
    /// rustc's `parse_in` once `parse_attribute` read the file's first attribute whole, up to a
    /// `]` before the one around the attribute as given: `unexpected` (`expected_one_of_not_found`
    /// expecting nothing) at the token after it. `None` when the attribute is not closed there,
    /// or the parser found an error in it, which is then the error.
    fn unexpected_after_attr(&self, errors: &[SyntaxError]) -> Option<Diag> {
        let attr = ast::SourceFile::cast(self.tree)?.attrs().next()?;
        attr.excl_token()?;
        let end = attr.r_brack_token()?.text_range().end();
        if errors.iter().any(|it| it.range().start() < end) {
            return None;
        }
        let (prev, token) = self.around(end);
        let (prev, token) = (prev?, token?);
        let span = self.at(token.range);
        // alloc: the error's text, for a crate attribute rustc refuses
        let message = format!("unexpected token: {}", self.describe(Some(token)));
        // clone: on the error path, for a crate attribute rustc refuses
        let mut diag = Diag::error(None, message, span.clone());
        let between = TextRange::new(
            token.range.end().min(prev.range.start()),
            token.range.end().max(prev.range.start()),
        );
        if !is_multiline(self.text, between) {
            diag.span_label(span, "unexpected token after this");
        } else {
            diag.span_label(self.at(prev.range), "unexpected token after this");
            diag.span_label(span, "unexpected token");
        }
        Some(diag)
    }
}
