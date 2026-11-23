//! A call written inside an expansion takes its input as tokens of that expansion (D122, "An
//! expansion is its token record"): a sub-range of the outer expansion's sealed stream, each
//! token keeping its context, so a `$crate` or a definition-site name passed down through any
//! number of calls keeps the mark it was written with. The text the outer expansion rendered
//! is read for one thing only: which tokens a call's input is, by the bytes the renderer
//! placed each token at (`resolve::render_with`). It is never lexed again.
//!
//! The key: [`blaze_expand::call_key`] hashes the call's input stream with every token's
//! context and the whole context table (`Call::encode`), so two inputs of equal text and other
//! contexts are two keys (rule 21). The outer expansion's own marks are sealed first
//! ([`TokenStream::seal`] with the outer call's key, as the body layer does in blaze-lower
//! `by_name.rs`), so the key names the expansion each mark came from and `Call::check` takes
//! the stream.

use core::fmt;

use blaze_expand::{Delim, TokenKind, TokenStream};
use blaze_own::{AVec, Alloc, BVec, Bump};

use crate::expansion_origins::offset_in;

/// An expansion's tokens as the calls written in its text read them: the stream sealed with
/// the call's key, where the renderer placed each token, and each token's first word in the
/// text as a lexer reads it back.
#[derive(Clone, Copy, Debug)]
pub struct ExpansionTokens<'t> {
    /// The expansion's stream, every own mark sealed as `MarkOf::Outer(call key)`.
    pub stream: &'t TokenStream<'t>,
    /// The rendered text the places index.
    pub text: &'t str,
    /// Per placed piece, in text order: `(start, end, token)`, the bytes of stream token
    /// `token` in `text` (a `$`+`crate` pair at its `$`; a `local_inner_macros` prefix is a
    /// piece of its own, of the token it stands before). Invisible delimiters are not placed.
    pub places: &'t [(u32, u32, u32)],
    /// Per stream token, the index of its first word in `text` lexed back (`None`: an
    /// invisible delimiter).
    pub words: &'t [Option<u32>],
}

/// How the call's input is cut from its text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputCut {
    /// The inside of a function-like call's delimiters (`input` is the text between them).
    Inside,
    /// The whole item an attribute, derive or keep call takes.
    Whole,
}

/// A call's input as a sub-range of the expansion it is written in.
#[derive(Clone, Copy, Debug)]
pub struct InputTokens<'t> {
    pub of: ExpansionTokens<'t>,
    /// The stream tokens `start..end`.
    pub start: u32,
    pub end: u32,
    /// The word of `text` the input's first word is, for the input's indexes as written.
    pub base: u32,
}

/// Where a call's input comes from.
#[derive(Clone, Copy, Debug)]
pub enum CallTokens<'t> {
    /// Text written in a file, lexed where it is read: a file's text has no tokens before it.
    Text,
    /// Tokens of the expansion the call is written in.
    Expansion(InputTokens<'t>),
    /// The call is written in an expansion, yet its input is no range of that expansion's
    /// tokens: refused when the call is expanded (rule 3), never lexed from its text.
    Refused(NestedInputError),
}

/// Why a call's input is no token range of the expansion it is written in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NestedInputError {
    /// The input is not a slice of the expansion's text.
    NotInText,
    /// No placed token opens the input at byte `at`.
    NoOpen { at: u32 },
    /// The token before the input (stream token `token`) is not a visible open delimiter.
    NotADelimiter { token: u32 },
    /// The group opened at stream token `token` is not closed.
    Unclosed { token: u32 },
    /// The item's bytes `start..end` hold no placed token.
    Empty { start: u32, end: u32 },
    /// The tokens of the item's bytes do not pair their delimiters (at stream token `token`).
    Unbalanced { token: u32 },
    /// The expansion's tables are past what they index.
    TooLarge,
}

impl fmt::Display for NestedInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = "a call written in an expansion takes its input as that expansion's tokens";
        match self {
            NestedInputError::NotInText => write!(f, "{what}: its input is not a slice of the expansion's text"),
            NestedInputError::NoOpen { at } => write!(f, "{what}: no token of the expansion opens the input at byte {at}"),
            NestedInputError::NotADelimiter { token } => write!(f, "{what}: token {token} before the input is no open delimiter"),
            NestedInputError::Unclosed { token } => write!(f, "{what}: the group opened at token {token} is not closed"),
            NestedInputError::Empty { start, end } => write!(f, "{what}: bytes {start}..{end} of the expansion hold no token"),
            NestedInputError::Unbalanced { token } => write!(f, "{what}: its tokens do not pair their delimiters at token {token}"),
            NestedInputError::TooLarge => write!(f, "{what}: the expansion is past what its tables index"),
        }
    }
}

impl core::error::Error for NestedInputError {}

impl<'t> ExpansionTokens<'t> {
    /// The tokens of the input `input` (a slice of [`ExpansionTokens::text`]) cut as `cut`.
    pub fn input(&self, input: &str, cut: InputCut) -> Result<InputTokens<'t>, NestedInputError> {
        let at = offset_in(self.text, input).ok_or(NestedInputError::NotInText)?;
        let end = at.checked_add(u32::try_from(input.len()).map_err(|_| NestedInputError::TooLarge)?).ok_or(NestedInputError::TooLarge)?;
        let tokens = self.stream.tokens();
        let (start, stop) = match cut {
            InputCut::Inside => {
                // `inside` takes the text after the open delimiter's one byte: the piece placed
                // last before `at` ends there and is that delimiter.
                let before = self.places.partition_point(|p| p.0 < at);
                let open = before.checked_sub(1).map(|i| self.places[i]).filter(|p| p.1 == at).ok_or(NestedInputError::NoOpen { at })?;
                let open = open.2;
                match tokens.get(open as usize).map(|t| t.kind) {
                    Some(TokenKind::Open(Delim::Paren | Delim::Brace | Delim::Bracket)) => {}
                    _ => return Err(NestedInputError::NotADelimiter { token: open }),
                }
                let close = self.stream.close_of(open as usize).ok_or(NestedInputError::Unclosed { token: open })?;
                let close = u32::try_from(close).map_err(|_| NestedInputError::TooLarge)?;
                (open + 1, close)
            }
            InputCut::Whole => {
                let first = self.places.partition_point(|p| p.0 < at);
                let inside = self.places[first..].iter().take_while(|p| p.1 <= end);
                let (mut lo, mut hi) = (u32::MAX, 0u32);
                for place in inside {
                    lo = lo.min(place.2);
                    // A `$`+`crate` pair is placed at its `$`: its `crate` is in the item too.
                    let span = if is_dollar_crate(self.stream, place.2 as usize) { 2 } else { 1 };
                    hi = hi.max(place.2 + span);
                }
                if lo == u32::MAX {
                    return Err(NestedInputError::Empty { start: at, end });
                }
                balanced(self.stream, lo, hi)?
            }
        };
        let base = match cut {
            // The input's words start after the open delimiter's one.
            InputCut::Inside => self.word(start - 1).map(|w| w + 1),
            InputCut::Whole => (start..stop).find_map(|t| self.word(t)),
        }
        .unwrap_or(0);
        Ok(InputTokens { of: *self, start, end: stop, base })
    }

    fn word(&self, token: u32) -> Option<u32> {
        self.words.get(token as usize).copied().flatten()
    }

    /// The same tokens placing `text`, a text written of `copies` of slices of
    /// [`ExpansionTokens::text`] (each `from` an offset there; sorted by `to`, as written): a
    /// statement call's statements read as items and a block's module text
    /// (crate::expanded_blocks `render`). Each placed piece wholly inside a copied slice moves
    /// with it; one of no slice is in no call of the new text. The words stay the expansion's:
    /// a call's input lies inside one copied slice (an item or a call is copied whole), so its
    /// words as written, counted from the input's first ([`InputTokens::written`]), are the same.
    ///
    /// Without it a call in such a text lexed its input from the text again, and a `$crate` of
    /// another crate in it, written `crate`, named the calling crate: antithesis_sdk's
    /// `assert_helper!` block, whose `#[$crate::linkme::distributed_slice($crate::assert::..)]`
    /// static gave linkme a bare `crate`, so its `crate::assert::ANTITHESIS_CATALOG! { .. }` was
    /// "no `assert` in `crate`" in every tempo member asserting (2026-11-17 g105938, 27 names).
    /// The Reference, "Macros By Example", "Hygiene": `$crate` names the crate of the macro that
    /// wrote it wherever its tokens end up.
    pub fn moved(&self, text: &'t str, copies: &[crate::crate_roots::Copied], alloc: Bump<'t>) -> ExpansionTokens<'t> {
        let mut places: BVec<'t, (u32, u32, u32)> = BVec::new_in(alloc);
        for copy in copies {
            let end = copy.from.saturating_add(copy.len);
            let first = self.places.partition_point(|p| p.0 < copy.from);
            for &(start, stop, token) in self.places[first..].iter().take_while(|p| p.0 < end) {
                if stop <= end {
                    places.push((start - copy.from + copy.to, stop - copy.from + copy.to, token));
                }
            }
        }
        ExpansionTokens { stream: self.stream, text, places: places.into_bump_slice(), words: self.words }
    }
}

/// Whether stream token `at` is a `$` the renderer wrote as one `$crate` with the next.
fn is_dollar_crate(stream: &TokenStream<'_>, at: usize) -> bool {
    matches!(stream.tokens().get(at).map(|t| t.kind), Some(TokenKind::Punct { char: '$', .. }))
        && stream.tokens().get(at + 1).is_some_and(|next| stream.text_of(next) == "crate")
}

/// `lo..hi` widened over the invisible delimiters its visible tokens sit in (an invisible
/// group writes no text, so the item's bytes cannot say whether it is inside): an unmatched
/// close widens back to its invisible open, an unclosed invisible open forward to its close.
/// A visible delimiter that does not pair is refused. Bounded by the stream's length.
fn balanced(stream: &TokenStream<'_>, mut lo: u32, mut hi: u32) -> Result<(u32, u32), NestedInputError> {
    let tokens = stream.tokens();
    if hi as usize > tokens.len() {
        return Err(NestedInputError::TooLarge);
    }
    // Back: closes in the range with no open in it, innermost first.
    let mut depth = 0i64;
    let mut need_back = 0u32;
    for at in lo..hi {
        match tokens[at as usize].kind {
            TokenKind::Open(_) => depth += 1,
            TokenKind::Close(_) if depth > 0 => depth -= 1,
            TokenKind::Close(Delim::Invisible) => need_back += 1,
            TokenKind::Close(_) => return Err(NestedInputError::Unbalanced { token: at }),
            _ => {}
        }
    }
    while need_back > 0 {
        lo = lo.checked_sub(1).ok_or(NestedInputError::Unbalanced { token: 0 })?;
        match tokens[lo as usize].kind {
            TokenKind::Open(Delim::Invisible) => need_back -= 1,
            _ => return Err(NestedInputError::Unbalanced { token: lo }),
        }
    }
    // Forward: opens left open at the end, each closed by an invisible close after it.
    let mut open: u32 = 0;
    let mut stack = 0u32;
    for at in lo..hi {
        match tokens[at as usize].kind {
            TokenKind::Open(_) => stack += 1,
            TokenKind::Close(_) => stack -= 1,
            _ => {}
        }
        open = stack;
    }
    while open > 0 {
        match tokens.get(hi as usize).map(|t| t.kind) {
            Some(TokenKind::Close(Delim::Invisible)) => {
                open -= 1;
                hi += 1;
            }
            _ => return Err(NestedInputError::Unbalanced { token: hi }),
        }
    }
    Ok((lo, hi))
}

impl<'t> InputTokens<'t> {
    /// The input as a stream of its own in `alloc`, each token's context imported.
    pub fn copy_in<'b>(&self, alloc: Alloc<'b>) -> Result<TokenStream<'b>, blaze_expand::StreamError> {
        self.of.stream.copy_in(self.start as usize..self.end as usize, alloc)
    }

    /// For each input token `k`, the index as written (the call's text lexed back, from its
    /// input's first word) of its first word: the origin column's input indexes
    /// ([`blaze_expand::origin_column::InputIndex::Aligned`]). An invisible delimiter, which
    /// writes nothing, is `None`.
    pub fn written<'b>(&self, alloc: Alloc<'b>) -> AVec<'b, Option<u32>> {
        let mut out = AVec::new_in(alloc);
        for token in self.start..self.end {
            out.push(self.of.word(token).and_then(|w| w.checked_sub(self.base)));
        }
        out
    }
}
