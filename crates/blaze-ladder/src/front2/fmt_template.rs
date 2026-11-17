//! Each `format_args!` template of a body as the code builder reads it
//! ([`blaze_build::Adts::format_template`]): its literal texts, escapes and `{{`/`}}`
//! processed, and its placeholders in order, from the lowering's `FormatArgs` row (blaze-lower
//! form.rs: the template's range, its `FmtPiece::Text` ranges in the template's SOURCE and its
//! `FmtPiece::Arg` spec ranges; fmt.rs `format_args_of` cuts them; `format_args_nl!` sets
//! `newline`, "a `\n` follows the last piece").
//!
//! A text range is the template's source: in a non-raw string its escapes are the string
//! literal's (the Reference, "String literals": `\n`, `\t`, `\\`, `\"`, `\x7F`, `\u{..}`, a
//! line continuation), in a raw one (`r"..."`, `r#"..."#`) there are none; in both `{{` is one
//! `{` and `}}` one `}` (core::fmt's doc, "Escaping"). Which it is is read from the expression's
//! text: the template is its first string token. A placeholder's spec (its text after `:`)
//! is unescaped the same way and handed on whole; the builder encodes its options.
//!
//! A template this cannot make (an escape the unescaper refuses, a text that is not UTF-8, a
//! range the view does not hold, a template not found as the expression's first string) has
//! no row: the builder refuses that `format_args!` by name ("whose template the world does not
//! give"), never a guessed text.

use std::alloc::Global;
use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use blaze_build::FmtPiece;
use blaze_lower::{Expr as LExpr, FmtPiece as LPiece, View};
use blaze_typeck::ExprId;

use super::body::Body;
use crate::front::string_bytes;

/// One template: its pieces, whose texts point into `text`.
pub(crate) struct Template {
    /// SAFETY: every `Text` here borrows from `text`'s heap buffer, which is never mutated,
    /// replaced or freed while `self` lives (the field is private and only read; moving the
    /// `Box` moves its pointer, never the buffer). The `'static` is never handed out:
    /// [`Template::pieces`] ties each borrow to `&self`.
    pieces: Box<[FmtPiece<'static>], Global>,
    text: Box<str, Global>,
}

impl Template {
    pub(crate) fn pieces(&self) -> &[FmtPiece<'_>] {
        let _ = &self.text;
        &self.pieces
    }
}

/// The pinned `rt::Argument` constructor for `t` (library/core/src/fmt/rt.rs, `impl
/// Argument<'_>`: `new_display<T: Display>(x: &T)`, `new_debug`, ...; `from_usize` for a
/// count): the same names as blaze-build walk/fmt.rs `argument_new_name`, which its refusal
/// prints.
pub(crate) const fn argument_new_name(t: blaze_typeck::FmtTrait) -> &'static str {
    use blaze_typeck::FmtTrait;
    match t {
        FmtTrait::Display => "new_display",
        FmtTrait::Debug => "new_debug",
        FmtTrait::LowerHex => "new_lower_hex",
        FmtTrait::UpperHex => "new_upper_hex",
        FmtTrait::Octal => "new_octal",
        FmtTrait::Binary => "new_binary",
        FmtTrait::LowerExp => "new_lower_exp",
        FmtTrait::UpperExp => "new_upper_exp",
        FmtTrait::Pointer => "new_pointer",
        FmtTrait::Count => "from_usize",
    }
}

/// Whether impl header text `header` (`impl Argument<'_>`, `impl<'a> Argument<'a>`) is an
/// inherent impl whose self type's path ends in `ty`: no ` for `, and after `impl` and its
/// generics the type's last segment is `ty`, followed by its arguments, `where` or the end.
pub(crate) fn inherent_on(header: &str, ty: &str) -> bool {
    let Some(mut rest) = header.trim().strip_prefix("impl") else { return false };
    if header.contains(" for ") {
        return false;
    }
    if rest.starts_with('<') {
        // The impl's generics: up to their matching `>`.
        let mut depth = 0usize;
        let mut end = None;
        for (at, c) in rest.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(at + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { return false };
        rest = &rest[end..];
    }
    let path = rest.trim_start();
    let path = path.split(|c: char| c == '<' || c.is_whitespace() || c == '{').next().unwrap_or("");
    path.rsplit("::").next() == Some(ty)
}

/// Every `format_args!` of `body` whose template could be made, by expression, sorted.
pub(crate) fn templates(body: &Body<'_>) -> Vec<(ExprId, Template), Global> {
    let view = body.lowered.view();
    let mut out = Vec::new_in(Global);
    for (e, x) in body.typeck.exprs.iter().enumerate() {
        let blaze_typeck::Expr::FormatArgs { .. } = x else { continue };
        let id = ExprId(e as u32);
        if let Some(t) = template(&view, id) {
            out.push((id, t));
        }
    }
    out
}

/// A piece before its text is placed: a range of the joined text, or a placeholder.
enum Part {
    Text(usize, usize),
    Arg(usize, usize),
}

fn template(view: &View<'_>, id: ExprId) -> Option<Template> {
    let LExpr::FormatArgs { row } = view.expr(id).ok()? else { return None };
    let f = view.format_args(view.row(row).ok()?).ok()?;
    let source = view.text_at(f.template).ok()?;
    let raw = is_raw(view.text_at(view.expr_range(id).ok()?).ok()?, source)?;
    let mut text = String::new();
    let mut parts: Vec<Part, Global> = Vec::new_in(Global);
    for &n in view.rows(f.pieces).ok()? {
        match view.fmt_piece(n).ok()? {
            LPiece::Text(r) => {
                let start = text.len();
                push_text(view.text_at(r).ok()?, raw, &mut text)?;
                if text.len() > start {
                    parts.push(Part::Text(start, text.len()));
                }
            }
            LPiece::Arg { spec, .. } => {
                // The spec's escapes are the template string's, processed the same way.
                let start = text.len();
                push_text(view.text_at(spec).ok()?, raw, &mut text)?;
                parts.push(Part::Arg(start, text.len()));
            }
        }
    }
    if f.newline {
        let start = text.len();
        text.push('\n');
        parts.push(Part::Text(start, text.len()));
    }
    let text: Box<str, Global> = text.into_boxed_str();
    let mut pieces = Vec::with_capacity_in(parts.len(), Global);
    for p in parts.iter() {
        pieces.push(match *p {
            Part::Text(s, e) => {
                let piece: *const str = text.get(s..e)?;
                // SAFETY: `piece` points into `text`'s heap buffer, which the `Template` made
                // below owns unchanged for as long as it holds `pieces` (its field doc).
                FmtPiece::Text(unsafe { &*piece })
            }
            Part::Arg(s, e) => {
                let spec: *const str = text.get(s..e)?;
                // SAFETY: as for `Text` above: `spec` points into `text`'s heap buffer, which
                // the `Template` made below owns unchanged for as long as it holds `pieces`.
                FmtPiece::Arg { spec: unsafe { &*spec } }
            }
        });
    }
    Some(Template { pieces: pieces.into_boxed_slice(), text })
}

/// Whether the template whose contents are `source` is a raw string in `whole`, the
/// expression's text: its first `"` opens the template (a `format_args!`'s first argument),
/// preceded by `r` and `#`s for a raw one. `None` when the contents do not follow that quote.
fn is_raw(whole: &str, source: &str) -> Option<bool> {
    let q = whole.find('"')?;
    if !whole.get(q + 1..)?.starts_with(source) {
        return None;
    }
    let before = whole.get(..q)?.trim_end_matches('#');
    let raw = before.ends_with('r') && !before[..before.len() - 1].ends_with(|c: char| c.is_alphanumeric() || c == '_');
    Some(raw)
}

/// Appends the output of template text `src` to `out`: `{{` and `}}` as one brace each, then
/// (in a non-raw string) its escapes processed.
fn push_text(src: &str, raw: bool, out: &mut String) -> Option<()> {
    let mut braces = String::with_capacity(src.len() + 2);
    if !raw {
        braces.push('"');
    }
    let mut rest = src;
    while let Some(c) = rest.chars().next() {
        let doubled = (c == '{' && rest.starts_with("{{")) || (c == '}' && rest.starts_with("}}"));
        braces.push(c);
        rest = &rest[if doubled { 2 } else { c.len_utf8() }..];
    }
    if raw {
        out.push_str(&braces);
        return Some(());
    }
    braces.push('"');
    let bytes = string_bytes(&braces)?;
    out.push_str(core::str::from_utf8(&bytes).ok()?);
    Some(())
}
