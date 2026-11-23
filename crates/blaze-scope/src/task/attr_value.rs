//! An attribute's `name = value` read to a string: the one place attribute values are read
//! (`#[lang]`, `#[path]`, and every attribute [`super::lower::Cfg::attr`] takes a string from).
//!
//! The Reference, "Attributes", "Meta Item Attribute Syntax": "Expressions in meta items must
//! macro-expand to literal expressions, which must not include integer or float type suffixes.
//! Expressions which are not literal expressions will be syntactically accepted (and can be
//! passed to proc-macros), but will be rejected after parsing." So a value that is a macro
//! call is expanded first and the attribute is read from the literal it gives:
//! library/core/src/panicking.rs (nightly-2026-10-27) declares its arithmetic panics as
//! `#[lang = stringify!($lang)] pub const fn $lang() -> ! { .. }` inside `panic_const!`.
//!
//! The lowering reads an expansion's text AFTER transcription (resolve.rs renders the
//! expansion's tokens, `$lang` already substituted, before the outline and the lowering read
//! it), so the attribute holds `stringify ! (panic_const_div_by_zero)`: the fix is this reader,
//! not the order.
//!
//! A quoted value is taken as before ([`super::lower::string_value`], borrowed from the text).
//! A macro call is kept as an [`AttrCall`] by [`super::lower::Cfg::attr`] (which has no edition
//! and no allocator) and read by [`read_call`] where the lowering has both: `stringify!` and
//! `concat!` are evaluated by the expander's own code ([`blaze_expand::attr_literal`]); any
//! other macro (`include_str!`, `env!`, one only name resolution can tell) is refused by name
//! ([`AttrValueRefusal`]), for `lang` and `path` alike.
//!
//! `#[doc = include_str!("../README.md")]`, `#[doc = concat!(..)]` and
//! `#[deprecated(note = concat!(..))]` are not read at all, as before: no reader of `doc` or
//! `deprecated` exists in the index (`item_attrs`, `file_attrs` and `Lower::attrs` skip doc
//! attributes, and `deprecated` reaches only `Attrs::others`'s inert names), and nothing in
//! the build depends on their value.

use core::fmt;

use blaze_expand::{AttrLiteralError, attr_literal};
use blaze_own::Bump;
use blaze_surface::Edition;

use super::lower::{head, string_value};

/// An attribute whose value is a macro call, as written: `attr` is its name (`lang`), `expr`
/// the value (`stringify ! (panic_const_div_by_zero)`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttrCall<'t> {
    pub attr: &'t str,
    pub expr: &'t str,
}

/// An attribute value that did not read to a string, naming the attribute, its value as
/// written and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttrValueRefusal<'t> {
    pub attr: &'t str,
    pub expr: &'t str,
    pub why: AttrLiteralError<'t>,
}

impl fmt::Display for AttrValueRefusal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "#[{} = {}]: {} (the Reference, \"Meta Item Attribute Syntax\": a value must macro-expand to a literal)",
            self.attr, self.expr, self.why
        )
    }
}

/// The string an attribute's content (`lang = "sized"`, `lang = stringify ! (x)`) holds:
/// a quoted value read now (as [`string_value`] always read it), or a macro call kept for
/// [`read_call`]. Neither for content without `=` or a value that is neither.
pub fn attr_string<'t>(content: &'t str) -> (Option<&'t str>, Option<AttrCall<'t>>) {
    let attr = head(content);
    let Some(rest) = content.get(attr.len()..) else { return (None, None) };
    let rest = rest.trim_start();
    let Some(value) = rest.strip_prefix('=').filter(|it| !it.starts_with('=')) else { return (None, None) };
    let expr = value.trim();
    // A call is a path and `!` before any quote: `"a!b"` is a string, `concat!("a")` a call.
    let bang = expr.find('!');
    let quote = expr.find('"');
    match (bang, quote) {
        (Some(bang), quote) if quote.is_none_or(|q| bang < q) => (None, Some(AttrCall { attr: attr.trim(), expr })),
        _ => (string_value(expr), None),
    }
}

/// The string `call`'s value expands to, lexed in the crate's `edition` and built in `alloc`
/// (a `stringify!` of one name is a copy; a `concat!` is new text, so the answer lives in
/// `alloc`, which outlives the text it is read beside).
pub fn read_call<'t, 'a: 't>(call: AttrCall<'t>, edition: Edition, alloc: Bump<'a>) -> Result<&'t str, AttrValueRefusal<'t>> {
    attr_literal(call.expr, edition, alloc).map_err(|why| AttrValueRefusal { attr: call.attr, expr: call.expr, why })
}
