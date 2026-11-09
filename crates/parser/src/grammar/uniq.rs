//! `&uniq` and `ref uniq` (`#![blaze::uniq]`, docs/v2/plan/design/edition.md, change 10).
//!
//! In a crate that says `#![blaze::uniq]` ([`crate::Extensions::UNIQ`]), `uniq` is a keyword
//! in exactly four places and an identifier everywhere else; in every other crate it is always
//! an identifier and the grammar is rustc's, token for token. Where it is the keyword it goes in
//! the tree as a `mut` token whose text is `uniq` (`Parser::bump_remap`), so the tree has the
//! shape of the `mut` spelling and everything after the parser reads one form of each.
//!
//! Which `uniq` is the keyword is told by the token after it, as `&raw const` and `&raw mut`
//! are told from a borrow of a variable named `raw`:
//! - a type, `&uniq T` and `&'a uniq T`: the next token starts a type and is none of `<`, `::`
//!   and `!`, which go on with a path or a type macro named `uniq` (`&uniq<T>`, `&uniq::X`,
//!   `&uniq!()`), nor a lifetime;
//! - an expression, `&uniq expr`: the next token starts an expression and is none of `<`, `::`,
//!   `!`, `.`, `{` and `#`, which go on with an expression `uniq` (a path, a macro call, a field
//!   or method, a range, a struct literal) or start an attribute;
//! - a receiver, `&uniq self` and `&'a uniq self`: `self` follows (and no `::`, as for `mut`);
//! - a binding, `ref uniq x`: an identifier follows.
//!
//! So `&uniq` before `;`, `,`, `)`, `]`, `}`, `.`, `::`, `<`, `=`, `as` or `?` is a shared borrow
//! of the variable `uniq`, as in rustc. Under the gate, a token that starts an expression and
//! could also go on with `uniq` (`-`, `*`, `&`, `&&`, `|`, `||`, `(`, `[`) starts the operand:
//! `&uniq - 1`, `&uniq * x`, `&uniq (x)` and `&uniq [0]`, which rustc reads as arithmetic on, a
//! call of or an index into `uniq`, are unique borrows of `-1`, `*x`, `(x)` and `[0]`, and
//! `&uniq (A)` in a type is a unique reference to `(A)`, not the `Fn`-style path `uniq(A)`: the
//! gate is the crate's opt-in to that reading. An operand that starts with one of the tokens
//! left out is written in parentheses: `&uniq (<T>::new())`.

use super::*;

/// The tokens after `uniq` that keep it the name of a type: a path's `<` or `::`, a type macro's
/// `!`, and a lifetime (no type starts with one).
const TYPE_GOES_ON: TokenSet = TokenSet::new(&[T![<], T![:], T![!], LIFETIME_IDENT]);

/// The tokens after `uniq` that keep it an expression of its own: a path's `<` or `::`, a macro
/// call's `!`, a field's, method's or range's `.`, a struct literal's `{`, and an attribute's
/// `#`.
const EXPR_GOES_ON: TokenSet =
    TokenSet::new(&[T![<], T![:], T![!], T![.], T!['{'], T![#]]);

/// Whether the parser, just past the `&` and lifetime of a reference type, is at the keyword
/// `uniq` of `&uniq T`.
pub(super) fn at_type(p: &Parser<'_>) -> bool {
    p.uniq_gate()
        && p.at_contextual_kw(T![uniq])
        && !TYPE_GOES_ON.contains(p.nth(1))
        && types::TYPE_FIRST.contains(p.nth(1))
}

/// Whether the parser, just past the `&` of a borrow, is at the keyword `uniq` of
/// `&uniq expr`.
pub(super) fn at_expr(p: &Parser<'_>) -> bool {
    p.uniq_gate()
        && p.at_contextual_kw(T![uniq])
        && !EXPR_GOES_ON.contains(p.nth(1))
        && expressions::EXPR_FIRST.contains(p.nth(1))
}

/// Whether token `n` is the `uniq` of a receiver `&uniq self`; the caller checks that an
/// isolated `self` follows it.
pub(super) fn nth_at_receiver(p: &Parser<'_>, n: usize) -> bool {
    p.uniq_gate() && p.nth_at_contextual_kw(n, T![uniq])
}

/// Whether the parser, just past the `ref` of a binding, is at the keyword `uniq` of
/// `ref uniq x`.
pub(super) fn at_ref_binding(p: &Parser<'_>) -> bool {
    p.uniq_gate() && p.at_contextual_kw(T![uniq]) && p.nth_at(1, IDENT)
}
