// Follows `compiler/rustc_ast/src/tokenstream.rs` at rust-lang/rust 12433c307 (MIT or Apache-2.0).

//! # Token Streams
//!
//! `TokenStream`s represent syntactic objects before they are converted into ASTs.
//! A `TokenStream` is, roughly speaking, a sequence of [`TokenTree`]s,
//! which are themselves a single [`Token`] or a `Delimited` subsequence of tokens.

use std::borrow::Cow;
use std::hash::Hash;
use std::mem;
use std::sync::{Arc, LazyLock};

use crate::ast::token::{self, Delimiter, Token, TokenKind};
use crate::span::{DUMMY_SP, Span};

/// Part of a `TokenStream`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenTree {
    /// A single token. Should never be `OpenDelim` or `CloseDelim`, because
    /// delimiters are implicitly represented by `Delimited`.
    Token(Token, Spacing),
    /// A delimited sequence of token trees.
    Delimited(DelimSpan, DelimSpacing, Delimiter, TokenStream),
}

impl TokenTree {
    /// Checks if this `TokenTree` is equal to the other, regardless of span/spacing information.
    pub fn eq_unspanned(&self, other: &TokenTree) -> bool {
        match (self, other) {
            (TokenTree::Token(token, _), TokenTree::Token(token2, _)) => token.kind == token2.kind,
            (TokenTree::Delimited(.., delim, tts), TokenTree::Delimited(.., delim2, tts2)) => {
                delim == delim2
                    && tts.len() == tts2.len()
                    && tts.iter().zip(tts2.iter()).all(|(a, b)| a.eq_unspanned(b))
            }
            _ => false,
        }
    }

    /// Retrieves the `TokenTree`'s span.
    pub fn span(&self) -> Span {
        match self {
            TokenTree::Token(token, _) => token.span,
            TokenTree::Delimited(sp, ..) => sp.entire(),
        }
    }

    /// Create a `TokenTree::Token` with alone spacing.
    pub fn token_alone(kind: TokenKind, span: Span) -> TokenTree {
        TokenTree::Token(Token::new(kind, span), Spacing::Alone)
    }

    /// Create a `TokenTree::Token` with joint spacing.
    pub fn token_joint(kind: TokenKind, span: Span) -> TokenTree {
        TokenTree::Token(Token::new(kind, span), Spacing::Joint)
    }

    /// Create a `TokenTree::Token` with joint-hidden spacing.
    pub fn token_joint_hidden(kind: TokenKind, span: Span) -> TokenTree {
        TokenTree::Token(Token::new(kind, span), Spacing::JointHidden)
    }

    pub fn uninterpolate(&self) -> Cow<'_, TokenTree> {
        match self {
            TokenTree::Token(token, spacing) => match token.uninterpolate() {
                Cow::Owned(token) => Cow::Owned(TokenTree::Token(token, *spacing)),
                Cow::Borrowed(_) => Cow::Borrowed(self),
            },
            _ => Cow::Borrowed(self),
        }
    }
}

/// The tokens rustc keeps of a node to re-expand it; rustfmt never reads them.
#[derive(Clone, Debug)]
pub struct LazyAttrTokenStream;

/// Indicates whether a token can join with the following token to form a
/// compound token. Used for conversions to `proc_macro::Spacing`. Also used to
/// guide pretty-printing, which is where the `JointHidden` value (which isn't
/// part of `proc_macro::Spacing`) comes in useful.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Spacing {
    /// The token cannot join with the following token to form a compound
    /// token.
    ///
    /// In token streams parsed from source code, the compiler will use `Alone`
    /// for any token immediately followed by whitespace, a non-doc comment, or
    /// EOF.
    ///
    /// When constructing token streams within the compiler, use this for each
    /// token that (a) should be pretty-printed with a space after it, or (b)
    /// is the last token in the stream. (In the latter case the choice of
    /// spacing doesn't matter because it is never used for the last token. We
    /// arbitrarily use `Alone`.)
    ///
    /// Converts to `proc_macro::Spacing::Alone`, and
    /// `proc_macro::Spacing::Alone` converts back to this.
    Alone,

    /// The token can join with the following token to form a compound token.
    ///
    /// In token streams parsed from source code, the compiler will use `Joint`
    /// for any token immediately followed by punctuation (as determined by
    /// `Token::is_punct`).
    ///
    /// When constructing token streams within the compiler, use this for each
    /// token that (a) should be pretty-printed without a space after it, and
    /// (b) is followed by a punctuation token.
    ///
    /// Converts to `proc_macro::Spacing::Joint`, and
    /// `proc_macro::Spacing::Joint` converts back to this.
    Joint,

    /// The token can join with the following token to form a compound token,
    /// but this will not be visible at the proc macro level. (This is what the
    /// `Hidden` means; see below.)
    ///
    /// In token streams parsed from source code, the compiler will use
    /// `JointHidden` for any token immediately followed by anything not
    /// covered by the `Alone` and `Joint` cases: an identifier, lifetime,
    /// literal, delimiter, doc comment.
    ///
    /// When constructing token streams, use this for each token that (a)
    /// should be pretty-printed without a space after it, and (b) is followed
    /// by a non-punctuation token.
    ///
    /// Converts to `proc_macro::Spacing::Alone`, but
    /// `proc_macro::Spacing::Alone` converts back to `token::Spacing::Alone`.
    /// Because of that, pretty-printing of `TokenStream`s produced by proc
    /// macros is unavoidably uglier (with more whitespace between tokens) than
    /// pretty-printing of `TokenStream`'s produced by other means (i.e. parsed
    /// source code, internally constructed token streams, and token streams
    /// produced by declarative macros).
    JointHidden,
}

/// A shared empty token stream, used to avoid an unnecessary `Arc` allocation for every empty
/// token stream.
static EMPTY_TOKEN_STREAM: LazyLock<TokenStream> =
    LazyLock::new(|| TokenStream(Arc::new(Vec::new())));

/// A `TokenStream` is an abstract sequence of tokens, organized into [`TokenTree`]s.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TokenStream(Arc<Vec<TokenTree>>);

impl TokenStream {
    pub fn new(tts: Vec<TokenTree>) -> TokenStream {
        if tts.is_empty() {
            TokenStream::default()
        } else {
            TokenStream(Arc::new(tts))
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn get(&self, index: usize) -> Option<&TokenTree> {
        self.0.get(index)
    }

    pub fn iter(&self) -> TokenStreamIter<'_> {
        TokenStreamIter::new(self)
    }

    /// Create a token stream containing a single token with alone spacing. The
    /// spacing used for the final token in a constructed stream doesn't matter
    /// because it's never used. In practice we arbitrarily use
    /// `Spacing::Alone`.
    pub fn token_alone(kind: TokenKind, span: Span) -> TokenStream {
        TokenStream::new(vec![TokenTree::token_alone(kind, span)])
    }

    // If `vec` is not empty, try to glue `tt` onto its last token. The return
    // value indicates if gluing took place.
    fn try_glue_to_last(vec: &mut [TokenTree], tt: &TokenTree) -> bool {
        if let Some(TokenTree::Token(last_tok, Spacing::Joint | Spacing::JointHidden)) = vec.last()
            && let TokenTree::Token(tok, spacing) = tt
            && let Some(glued_tok) = last_tok.glue(tok)
        {
            // ...then overwrite the last token tree in `vec` with the glued token.
            *vec.last_mut().unwrap() = TokenTree::Token(glued_tok, *spacing);
            true
        } else {
            false
        }
    }

    /// Push `tt` onto the end of the stream, possibly gluing it to the last
    /// token. Uses `make_mut` to maximize efficiency.
    ///
    /// This is intended for specific proc macro use. For general `TokenStream`
    /// construction within the compiler just build a `Vec<TokenTree>` with
    /// normal `Vec` operations and then do `TokenStream::new`.
    pub fn push_tree_with_gluing(&mut self, tt: TokenTree) {
        let vec_mut = Arc::make_mut(&mut self.0);

        if Self::try_glue_to_last(vec_mut, &tt) {
            // nothing else to do
        } else {
            vec_mut.push(tt);
        }
    }

    /// Push `stream` onto the end of the stream, possibly gluing the first
    /// token tree to the last token. (No other token trees will be glued.)
    /// Uses `make_mut` to maximize efficiency.
    ///
    /// This is intended for specific proc macro use. For general `TokenStream`
    /// construction within the compiler just build a `Vec<TokenTree>` with
    /// normal `Vec` operations and then do `TokenStream::new`.
    pub fn push_stream_with_gluing(&mut self, stream: &TokenStream) {
        let vec_mut = Arc::make_mut(&mut self.0);

        let stream_iter = stream.0.iter().cloned();

        if let Some(first) = stream.0.first()
            && Self::try_glue_to_last(vec_mut, first)
        {
            // Now skip the first token tree from `stream`.
            vec_mut.extend(stream_iter.skip(1));
        } else {
            // Append all of `stream`.
            vec_mut.extend(stream_iter);
        }
    }

    /// Desugar doc comments like `/// foo` in the stream into `#[doc =
    /// r"foo"]`. Modifies the `TokenStream` via `Arc::make_mut`, but as little
    /// as possible.
    /// Given a `TokenStream` with a `Stream` of only two arguments, return a new `TokenStream`
    /// separating the two arguments with a comma for diagnostic suggestions.
    pub fn add_comma(&self) -> Option<(TokenStream, Span)> {
        // Used to suggest if a user writes `foo!(a b);`
        let mut suggestion = None;
        let mut iter = self.0.iter().enumerate().peekable();
        while let Some((pos, ts)) = iter.next() {
            if let Some((_, next)) = iter.peek() {
                let sp = match (&ts, &next) {
                    (
                        _,
                        TokenTree::Token(
                            Token {
                                kind: token::Comma, ..
                            },
                            _,
                        ),
                    ) => continue,
                    (
                        TokenTree::Token(token_left, Spacing::Alone),
                        TokenTree::Token(token_right, _),
                    ) if (token_left.is_non_reserved_ident() || token_left.is_lit())
                        && (token_right.is_non_reserved_ident() || token_right.is_lit()) =>
                    {
                        token_left.span
                    }
                    (TokenTree::Delimited(sp, ..), _) => sp.entire(),
                    _ => continue,
                };
                let sp = sp.shrink_to_hi();
                let comma = TokenTree::token_alone(token::Comma, sp);
                suggestion = Some((pos, comma, sp));
            }
        }
        if let Some((pos, comma, sp)) = suggestion {
            let mut new_stream = Vec::with_capacity(self.0.len() + 1);
            let parts = self.0.split_at(pos + 1);
            new_stream.extend_from_slice(parts.0);
            new_stream.push(comma);
            new_stream.extend_from_slice(parts.1);
            return Some((TokenStream::new(new_stream), sp));
        }
        None
    }
}

impl Default for TokenStream {
    fn default() -> TokenStream {
        EMPTY_TOKEN_STREAM.clone()
    }
}

impl FromIterator<TokenTree> for TokenStream {
    fn from_iter<I: IntoIterator<Item = TokenTree>>(iter: I) -> Self {
        TokenStream::new(iter.into_iter().collect::<Vec<TokenTree>>())
    }
}

#[derive(Clone)]
pub struct TokenStreamIter<'t>(std::slice::Iter<'t, TokenTree>);

impl<'t> TokenStreamIter<'t> {
    fn new(stream: &'t TokenStream) -> Self {
        TokenStreamIter(stream.0.as_slice().iter())
    }

    // Peeking could be done via `Peekable`, but most iterators need peeking,
    // and this is simple and avoids the need to use `peekable` and `Peekable`
    // at all the use sites.
    pub fn peek(&self) -> Option<&'t TokenTree> {
        self.0.as_slice().first()
    }
}

impl<'t> Iterator for TokenStreamIter<'t> {
    type Item = &'t TokenTree;

    fn next(&mut self) -> Option<&'t TokenTree> {
        self.0.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

#[derive(Clone, Debug)]
struct TokenTreeCursor {
    stream: TokenStream,
    /// Points to the next token tree (or one past the end of the stream).
    next_idx: usize,
}

impl TokenTreeCursor {
    #[inline]
    fn new(stream: TokenStream) -> Self {
        TokenTreeCursor {
            stream,
            next_idx: 0,
        }
    }

    /// Gets the current token tree within this cursor. In a debug build it panics on a cursor that
    /// hasn't been bumped; in a release build it will return `None`.
    #[inline]
    fn curr(&self) -> Option<&TokenTree> {
        debug_assert!(self.next_idx > 0);
        self.stream.get(self.next_idx - 1)
    }

    /// Gets the next token tree without advancing.
    #[inline]
    fn next(&self) -> Option<&TokenTree> {
        self.stream.get(self.next_idx)
    }

    /// Gets the token tree `n` ahead. `look_ahead(1)` is equivalent to `next()`. `look_ahead(0)`
    /// isn't allowed and will panic.
    #[inline]
    fn look_ahead(&self, n: usize) -> Option<&TokenTree> {
        assert_ne!(n, 0);
        self.stream.get(self.next_idx + (n - 1))
    }

    /// Move the cursor to the next token tree.
    #[inline]
    fn bump(&mut self) {
        self.next_idx += 1;
    }

    /// For skipping ahead in rare circumstances.
    #[inline]
    fn bump_to_end(&mut self) {
        self.next_idx = self.stream.len();
    }
}

/// A `TokenStream` cursor that produces `Token`s. It's a bit odd that
/// we (a) lex tokens into a nice tree structure (`TokenStream`), and then (b)
/// use this type to emit them as a linear sequence. But a linear sequence is
/// what the parser expects, for the most part.
#[derive(Clone, Debug)]
pub struct TokenCursor {
    // Cursor for the current (innermost) token stream. The `next_idx` within the
    // cursor can point to any token tree in the stream (or one past the end).
    // The delimiters for this token stream are found in the current token tree
    // in `self.stack.last()`; if that is `None` we are in the outermost token
    // stream which never has delimiters.
    curr: TokenTreeCursor,

    // Token streams surrounding the current one. The `next_idx` within each cursor
    // is always greater than zero and always points one past the current
    // `TokenTree::Delimited`.
    stack: Vec<TokenTreeCursor>,
}

impl TokenCursor {
    #[inline]
    pub fn new(stream: TokenStream) -> Self {
        TokenCursor {
            curr: TokenTreeCursor::new(stream),
            stack: vec![],
        }
    }

    /// Gets the next token and advances the cursor by one.
    pub fn next_and_bump(&mut self) -> (Token, Spacing) {
        self.inlined_next_and_bump()
    }

    /// An `n` of 1 is the next token tree in the current token stream; won't look outside the
    /// current token stream. `look_ahead(0)` isn't allowed and will panic.
    #[inline]
    pub fn look_ahead(&self, n: usize) -> Option<&TokenTree> {
        self.curr.look_ahead(n)
    }

    /// Returns the first token tree (if there is one) past the close delimiter of the enclosing
    /// delimited sequence. Panics if we are not within a delimited sequence.
    #[inline]
    pub fn look_ahead_past_close_delim(&self) -> Option<&TokenTree> {
        self.stack.last().unwrap().next()
    }

    /// Clones the `TokenTree::Delimited` that we are currently within. Panics if we are not within
    /// a delimited sequence.
    #[inline]
    pub fn clone_enclosing_delim(&self) -> TokenTree {
        self.stack.last().unwrap().curr().unwrap().clone()
    }

    /// For skipping to the end of the current sequence, in rare circumstances.
    #[inline]
    pub fn bump_to_end(&mut self) {
        self.curr.bump_to_end()
    }

    /// Note: the outermost stream has depth of 0.
    #[inline]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// Returns details about the parent delimited sequence, if there is one.
    #[inline]
    pub fn parent_delim_and_span(&self) -> Option<(Delimiter, DelimSpan)> {
        if let Some(last) = self.stack.last()
            && let Some(TokenTree::Delimited(span, _, delim, _)) = last.curr()
        {
            Some((*delim, *span))
        } else {
            None
        }
    }

    /// This always-inlined version should only be used on hot code paths.
    #[inline(always)]
    pub fn inlined_next_and_bump(&mut self) -> (Token, Spacing) {
        loop {
            // FIXME: we currently don't return `Delimiter::Invisible` open/close delims. To fix
            // #67062 we will need to, whereupon the `delim != Delimiter::Invisible` conditions
            // below can be removed.
            if let Some(tree) = self.curr.next() {
                match tree {
                    &TokenTree::Token(token, spacing) => {
                        debug_assert!(!token.kind.is_delim());
                        let res = (token, spacing);
                        self.curr.bump();
                        return res;
                    }
                    &TokenTree::Delimited(sp, spacing, delim, ref tts) => {
                        let trees = TokenTreeCursor::new(tts.clone());
                        self.curr.bump(); // move past the `Delimited`
                        self.stack.push(mem::replace(&mut self.curr, trees));
                        if !delim.skip() {
                            return (
                                Token::new(delim.as_open_token_kind(), sp.open),
                                spacing.open,
                            );
                        }
                        // No open delimiter to return; continue on to the next iteration.
                    }
                };
            } else if let Some(parent) = self.stack.pop() {
                // We have exhausted this token stream. Move back to its parent token stream.
                let Some(&TokenTree::Delimited(span, spacing, delim, _)) = parent.curr() else {
                    panic!("parent should be Delimited")
                };
                self.curr = parent;
                if !delim.skip() {
                    return (
                        Token::new(delim.as_close_token_kind(), span.close),
                        spacing.close,
                    );
                }
                // No close delimiter to return; continue on to the next iteration.
            } else {
                // We have exhausted the outermost token stream. The use of
                // `Spacing::Alone` is arbitrary and immaterial, because the
                // `Eof` token's spacing is never used.
                return (Token::new(token::Eof, DUMMY_SP), Spacing::Alone);
            }
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]

pub struct DelimSpan {
    pub open: Span,
    pub close: Span,
}

impl DelimSpan {
    pub fn from_single(sp: Span) -> Self {
        DelimSpan {
            open: sp,
            close: sp,
        }
    }

    pub fn from_pair(open: Span, close: Span) -> Self {
        DelimSpan { open, close }
    }

    pub fn dummy() -> Self {
        Self::from_single(DUMMY_SP)
    }

    pub fn entire(self) -> Span {
        self.open.with_hi(self.close.hi())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DelimSpacing {
    pub open: Spacing,
    pub close: Spacing,
}

impl DelimSpacing {
    pub fn new(open: Spacing, close: Spacing) -> DelimSpacing {
        DelimSpacing { open, close }
    }
}
