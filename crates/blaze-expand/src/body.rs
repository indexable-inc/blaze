//! A macro call inside a function body, and what its expansion is to the lowering that reads
//! it in place (blaze-lower): the fragment its position takes, the expansion's tokens as the
//! parser's words, and each token's local hygiene.
//!
//! # Local hygiene (`macro_rules!`)
//!
//! Rust's rule for `macro_rules!` is mixed-site hygiene: a local variable, a label and `$crate`
//! resolve where the macro is defined, every other name where it is called. So a `let x`
//! written in a macro's body binds an `x` that only the same expansion's own `x` tokens see,
//! and never captures, nor is captured by, an `x` of the caller's, while an `x` the caller
//! passes in as `$e` is the caller's `x` on both sides.
//!
//! The representation: a token's context is a chain of marks, one per expansion it passed
//! through ([`crate::CtxNode`]). Its [`local_hygiene`] is a hash of the marks in that chain
//! that are not [`crate::Transparency::Transparent`] (a `macro_rules!` mark is semi-opaque,
//! a `macro` mark opaque, both count; a transparent one, a proc macro's call-site span, does
//! not), each named by its expansion's key, the call's own (`MarkOf::This`) by the key of the
//! call being lowered. A token of no such mark has 0. Two local names are the same local
//! when their text and their local hygiene are equal: the lowering keys its locals and
//! labels by both, nothing else changes.

use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_own::{AString, AVec, Alloc, Vec};

use crate::origin_column::TextMark;
use crate::tokens::{Ctx, Delim, LitKind, MarkOf, Raw, Token, TokenKind, Transparency, TokenStream};

/// What a call's position takes, and so what its expansion is parsed as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fragment {
    /// An expression: `let x = m!(..);`, `f(m!(..))`.
    Expr,
    /// Statements, the last of which may be an expression with no `;`: a call that is a
    /// statement of a block.
    Stmts,
    /// A pattern.
    Pat,
    /// A type.
    Type,
    /// Items: a call at item position.
    Items,
}

impl Fragment {
    /// The parser's entry point for the fragment.
    pub fn entry(self) -> parser::TopEntryPoint {
        match self {
            Fragment::Expr => parser::TopEntryPoint::Expr,
            Fragment::Stmts => parser::TopEntryPoint::MacroStmts,
            Fragment::Pat => parser::TopEntryPoint::Pattern,
            Fragment::Type => parser::TopEntryPoint::Type,
            Fragment::Items => parser::TopEntryPoint::MacroItems,
        }
    }
}

/// An expansion's tokens as the parser reads them.
pub struct ParserWords<'a> {
    /// One word per parser token, then [`parser::Tokens::PAD`] words of `EOF`: read with
    /// `parser::Tokens::new(&words, true)`.
    pub words: AVec<'a, u32>,
    /// For each token of the stream, the word it starts (a lifetime's name and an invisible
    /// group's delimiters start none of their own); one past the last at the stream's length.
    pub word_at: AVec<'a, u32>,
}

/// The parser's words for `stream`, in `alloc`.
pub fn parser_words<'a>(stream: &TokenStream<'_>, alloc: Alloc<'a>) -> ParserWords<'a> {
    let mut words = Vec::new_in(alloc);
    let mut word_at = Vec::new_in(alloc);
    let tokens = stream.tokens();
    crate::rules::words_of(
        crate::tokens::index(tokens.len()),
        |at| {
            let token = tokens[at as usize];
            (token.kind, stream.text_of(&token), stream.edition_of(token.ctx))
        },
        &mut words,
        &mut word_at,
    );
    ParserWords { words, word_at }
}

/// The local hygiene of a token of `ctx` in `stream` (see the module's doc): 0 when no
/// semi-opaque or opaque mark is on it; `this` names the stream's own expansion's marks.
pub fn local_hygiene(stream: &TokenStream<'_>, mut ctx: Ctx, this: ContentId) -> u64 {
    let mut hasher: Option<Hasher> = None;
    while let Some(node) = stream.node(ctx) {
        if node.mark.transparency != Transparency::Transparent {
            let key = match node.mark.of {
                MarkOf::This => this,
                MarkOf::Outer(key) => key,
            };
            let h = hasher.get_or_insert_with(|| Hasher::new("blaze v2 local hygiene 1"));
            h.id(&key);
            h.part(&[u8::from(node.mark.transparency == Transparency::Opaque)]);
        }
        ctx = node.parent;
    }
    match hasher {
        None => 0,
        Some(h) => {
            let id = h.finish();
            let mut first = [0u8; 8];
            first.copy_from_slice(&id.0[..8]);
            // Never 0, which is a token of no mark's.
            u64::from_le_bytes(first) | 1
        }
    }
}

/// One parser word of an expansion: its source text (a range of [`BodyWords::text`]), the
/// stream's tokens it is (`token..end`: two for a lifetime, `'` and its name, else one), and
/// its [`local_hygiene`] (a lifetime's is its name's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyWord {
    pub text: (u32, u32),
    pub token: u32,
    /// One past the word's last token.
    pub end: u32,
    pub hygiene: u64,
    /// The innermost mark of the word's token (a lifetime's: its name's), `None` for a token
    /// of the call as written: where a path starting at this word resolves (drive.md, "A token
    /// keeps its mark"; blaze-ladder front2 world_paths.rs `site_of`).
    pub mark: Option<TextMark>,
    /// Whether a space may go before the word's text: not after a joint punctuation, which
    /// would no longer glue to it when the text is lexed again (`::`, `->`, `..=`).
    pub space_before: bool,
}

/// An expansion as a body's lowering reads it in place: the parser's words, and for each of
/// them (in order, one each) its source text and local hygiene.
pub struct BodyWords<'a> {
    pub parser: ParserWords<'a>,
    pub words: AVec<'a, BodyWord>,
    pub text: AString<'a>,
}

impl<'a> BodyWords<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> BodyWords<'a> {
        BodyWords {
            parser: ParserWords { words: Vec::new_in(alloc), word_at: Vec::new_in(alloc) },
            words: Vec::new_in(alloc),
            text: AString::new_in(alloc),
        }
    }
}

/// `stream` as [`BodyWords`], its own marks named by `this` (the key of the call it expands).
pub fn body_words<'a>(stream: &TokenStream<'_>, this: ContentId, alloc: Alloc<'a>) -> BodyWords<'a> {
    let parser = parser_words(stream, alloc);
    let tokens = stream.tokens();
    let mut words: AVec<'a, BodyWord> = Vec::with_capacity_in(tokens.len(), alloc);
    let mut text = AString::new_in(alloc);
    let mut glued = false;
    // The token before was a lifetime's `'`, which started the word its name ends.
    let mut after_quote = false;
    for (at, token) in tokens.iter().enumerate() {
        // A lifetime is two tokens, `'` and its name, and one parser word (`words_of`: the
        // `'` takes the next token, whatever it is, as its name): the word starts at the `'`,
        // so its text is the whole `'name` the lowering keys a label by, and its hygiene is
        // the name's. Any other token starts a word when the next token's word is a later
        // one: an invisible group's delimiters start none and join the word before.
        let name_of_lifetime = after_quote;
        let quote = !name_of_lifetime
            && matches!(token.kind, TokenKind::Punct { char: '\'', .. })
            && at + 1 < tokens.len();
        after_quote = quote;
        let starts = quote || (!name_of_lifetime && parser.word_at[at + 1] > parser.word_at[at]);
        if starts {
            let start = crate::tokens::index(text.len());
            let ctx = match quote {
                true => tokens[at + 1].ctx,
                false => token.ctx,
            };
            words.push(BodyWord {
                text: (start, start),
                token: crate::tokens::index(at),
                end: crate::tokens::index(at + if quote { 2 } else { 1 }),
                hygiene: local_hygiene(stream, ctx, this),
                mark: stream.hygiene(ctx).mark.map(TextMark::of),
                space_before: !glued,
            });
        }
        if !is_invisible(token.kind) {
            glued = glues(token.kind);
        }
        // `$crate` "refers to the crate defining the macro" (the Reference, "Macros By
        // Example", Hygiene): written as `$crate`; its crate is its word's mark
        // (`BodyWord::mark`), which the lowering keeps beside the text.
        write_token(stream, token, &mut text);
        if let Some(word) = words.last_mut() {
            word.text.1 = crate::tokens::index(text.len());
        }
    }
    BodyWords { parser, words, text }
}

/// Whether a renderer that keeps the stream's spacing writes no space between `written` and
/// `next`, the token written after it: `written` is not `Alone` (it touched `next` where it
/// was read), unless the two would lex back as another token. The one such pair is an integer
/// with no suffix before a `.`: rustc's lexer reads `999.` that a space or an operator follows
/// as the float `999.`, so core's `999.try_into()`, whose `.` is `Alone` before an identifier,
/// would be written `999. try_into()` and not parse; `999 .try_into()` lexes as its tokens.
/// blaze-scope resolve.rs `render_with` (an item expansion's text) asks it.
pub fn glued_in_text(written: TokenKind, next: TokenKind) -> bool {
    if let (TokenKind::Literal { kind: LitKind::Integer, suffix_len: 0, .. }, TokenKind::Punct { char: '.', .. }) = (written, next) {
        return false;
    }
    !matches!(written.spacing(), crate::tokens::Spacing::Alone)
}

/// A punctuation that glues to the next token: no space may go between them.
fn glues(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Punct { spacing: crate::tokens::Spacing::Joint, .. })
}

fn is_invisible(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible))
}

/// Writes `stream` as source text to `out`: what lexes back to its tokens, a space between
/// two tokens unless the first is a joint punctuation.
pub fn write_tokens(stream: &TokenStream<'_>, out: &mut AString<'_>) {
    let mut glued = true;
    for token in stream.tokens() {
        if is_invisible(token.kind) {
            continue;
        }
        if !glued {
            out.push(' ');
        }
        glued = glues(token.kind);
        write_token(stream, token, out);
    }
}

/// Writes the source text of `token`, one of `stream`'s, to `out`: what lexes back to it.
pub fn write_token(stream: &TokenStream<'_>, token: &Token, out: &mut AString<'_>) {
    let text = stream.text_of(token);
    match token.kind {
        TokenKind::Ident { raw, .. } => {
            if raw == Raw::Yes {
                out.push_str("r#");
            }
            out.push_str(text);
        }
        TokenKind::Punct { char, .. } => out.push(char),
        TokenKind::Open(delim) => out.push_str(match delim {
            Delim::Paren => "(",
            Delim::Brace => "{",
            Delim::Bracket => "[",
            Delim::Invisible => "",
        }),
        TokenKind::Close(delim) => out.push_str(match delim {
            Delim::Paren => ")",
            Delim::Brace => "}",
            Delim::Bracket => "]",
            Delim::Invisible => "",
        }),
        TokenKind::Literal { kind, suffix_len, .. } => {
            let (prefix, quote, hashes) = match kind {
                LitKind::Integer | LitKind::Float | LitKind::Err => {
                    out.push_str(text);
                    return;
                }
                LitKind::Byte => ("b", '\'', 0),
                LitKind::Char => ("", '\'', 0),
                LitKind::Str => ("", '"', 0),
                LitKind::StrRaw(n) => ("r", '"', n),
                LitKind::ByteStr => ("b", '"', 0),
                LitKind::ByteStrRaw(n) => ("br", '"', n),
                LitKind::CStr => ("c", '"', 0),
                LitKind::CStrRaw(n) => ("cr", '"', n),
            };
            let (body, suffix) = text.split_at(text.len() - suffix_len as usize);
            out.push_str(prefix);
            (0..hashes).for_each(|_| out.push('#'));
            out.push(quote);
            out.push_str(body);
            out.push(quote);
            (0..hashes).for_each(|_| out.push('#'));
            out.push_str(suffix);
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::AString;

    use super::{glued_in_text, write_token};
    use crate::tokens::{Edition, LitKind, TokenKind, lex};

    /// core's `_fmt_inner` (library/core/src/fmt/num.rs) holds `999.try_into()` and
    /// `1_00_00.try_into()`: written with the stream's spacing they read `999. try_into ..`,
    /// the float `999.` then a name, and the expansion of `impl_Display!` did not parse
    /// (`Diag { kind: Syntax }` at the byte after `999.`). Written by [`glued_in_text`] they
    /// lex back to the same tokens.
    ///
    /// Prints: `test body::tests::an_integer_before_a_dot_stays_an_integer ... ok`. A wrong
    /// version (glued by spacing alone) fails with "the integer lexed back as Float".
    #[test]
    fn an_integer_before_a_dot_stays_an_integer() {
        let alloc = blaze_own::testing::heap().alloc();
        for text in ["999.try_into()", "1_00_00.try_into()", "0x1f.min(3)", "x.0.1", "1..2", "a.b"] {
            let stream = lex(text, Edition::E2024, alloc).expect("the text lexes");
            let tokens = stream.tokens();
            let mut out = AString::new_in(alloc);
            for (at, token) in tokens.iter().enumerate() {
                if at > 0 && !glued_in_text(tokens[at - 1].kind, token.kind) {
                    out.push(' ');
                }
                write_token(&stream, token, &mut out);
            }
            let again = lex(&out, Edition::E2024, alloc).expect("the written text lexes");
            assert_eq!(again.tokens().len(), tokens.len(), "{text} written as {out}");
            for (was, now) in tokens.iter().zip(again.tokens()) {
                let kind = |k: TokenKind| match k {
                    TokenKind::Literal { kind, .. } => Some(kind),
                    _ => None,
                };
                assert_eq!(kind(now.kind), kind(was.kind), "{text} written as {out}: the integer lexed back as {:?}", kind(now.kind));
            }
        }
        // `1..2` keeps its range: the integer's next token is `.`, and a space before it
        // changes nothing.
        let stream = lex("1..2", Edition::E2024, alloc).expect("the text lexes");
        assert!(matches!(stream.tokens()[0].kind, TokenKind::Literal { kind: LitKind::Integer, .. }));
    }
}
