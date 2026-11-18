//! The one home of how an attribute's or a derive's path is read from tokens and compared: word
//! by word, by structure, never by its spelling as text.
//!
//! An attribute call is found on its item by the path the lowering read for it. The lowering
//! reads a file's text or an expansion's rendered text, where a `$crate` is written `crate` and
//! every token is spaced (`crate:: linkme:: distributed_slice`); the item walk reads tokens,
//! where the transcriber's `$crate` is one identifier `$crate` (rules/transcribe.rs `Op::Crate`)
//! and a `$crate` lexed from text is a `$` then `crate`. Compared as text with the spaces taken
//! out, `crate::linkme::distributed_slice` is not `$crate::linkme::distributed_slice`: an item a
//! `macro_rules!` body wrote as `#[$crate::linkme::distributed_slice(..)] static ..` (antithesis
//! sdk's registration macros, linkme's README form) was refused as "not among the item's
//! attributes" in every tempo member using it (2026-11-17, 27 calls in 8 files).
//!
//! Here a path is its words: a leading `::`, a crate root (`crate`, `$crate` as one token, or a
//! `$` then `crate`), and names. Which crate a crate root names is not read: the lowering
//! resolved the path already by its mark, and the walk only finds which of the item's
//! attributes it read. That is the first attribute equal word by word: whether an attribute
//! takes the item is a function of its words (its first segment's tool, a one-segment name's
//! builtin set), so an earlier attribute of equal words would have been the lowering's taker.
//!
//! The Reference, "Macros By Example", "Hygiene": `$crate` refers to the crate defining the
//! macro; "Attributes": an attribute's path is a SimplePath (`::`? segment (`::` segment)*,
//! a segment being an identifier, `super`, `self`, `crate` or `$crate`).

use core::ops::Range;

use crate::tokens::{Raw, TokenKind, TokenStream};

/// One word of a path, as [`path_words`] reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathWord<'s> {
    /// A `::` before the first segment: the path is from the extern prelude.
    LeadingColons,
    /// `crate` or `$crate` as the first segment, however `$crate` is held (one token, or a `$`
    /// then `crate`).
    CrateRoot,
    /// Any other segment: an identifier's name (without `r#`), `self` or `super`.
    Name(&'s str),
    /// A token no path holds: a path with one is equal to none.
    NotAPath,
}

/// The token after the path that starts at token `at` of `stream` (`::`? head (`::` name)*,
/// the head possibly a `$` then `crate`); `None` when no path starts there.
pub(crate) fn path_end(stream: &TokenStream<'_>, at: usize) -> Option<usize> {
    let tokens = stream.tokens();
    let punct = |at: usize, want: char| matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Punct { char, .. }) if char == want);
    let ident = |at: usize| matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Ident { .. }));
    let sep = |at: usize| punct(at, ':') && punct(at + 1, ':');
    let mut at = at;
    if sep(at) {
        at += 2;
    } else if dollar_then_crate(stream, at) {
        // The `$`; its `crate` is the first segment read next.
        at += 1;
    }
    loop {
        if !ident(at) {
            return None;
        }
        at += 1;
        if !sep(at) {
            return Some(at);
        }
        at += 2;
    }
}

/// Whether token `at` of `stream` is a `$` followed by the identifier `crate` (a `$crate` lexed
/// from text).
fn dollar_then_crate(stream: &TokenStream<'_>, at: usize) -> bool {
    let tokens = stream.tokens();
    matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Punct { char: '$', .. }))
        && matches!(tokens.get(at + 1).map(|t| t.kind), Some(TokenKind::Ident { raw: Raw::No, .. }))
        && stream.text_at(at + 1) == "crate"
}

/// The words of the tokens `range` of `stream` read as a path.
pub struct PathWords<'s, 't> {
    stream: &'s TokenStream<'t>,
    at: usize,
    end: usize,
    first: bool,
}

/// The words of the tokens `range` of `stream`, read as a path ([`PathWord`]). A range past
/// the stream ends where the stream does.
pub fn path_words<'s, 't>(stream: &'s TokenStream<'t>, range: Range<usize>) -> PathWords<'s, 't> {
    let end = range.end.min(stream.len());
    PathWords { stream, at: range.start, end, first: true }
}

impl<'s> Iterator for PathWords<'s, '_> {
    type Item = PathWord<'s>;

    fn next(&mut self) -> Option<PathWord<'s>> {
        let stream: &'s TokenStream<'_> = self.stream;
        let tokens = stream.tokens();
        while self.at < self.end {
            let at = self.at;
            let first = self.first;
            self.first = false;
            let colon = |at: usize| matches!(tokens.get(at).map(|t| t.kind), Some(TokenKind::Punct { char: ':', .. }));
            match tokens[at].kind {
                TokenKind::Punct { char: ':', .. } if at + 1 < self.end && colon(at + 1) => {
                    self.at += 2;
                    if first {
                        return Some(PathWord::LeadingColons);
                    }
                    // A separator between segments is no word.
                }
                TokenKind::Punct { char: '$', .. } if first && at + 1 < self.end && dollar_then_crate(stream, at) => {
                    self.at += 2;
                    return Some(PathWord::CrateRoot);
                }
                TokenKind::Ident { raw: Raw::No, .. } if first && matches!(stream.text_at(at), "crate" | "$crate") => {
                    self.at += 1;
                    return Some(PathWord::CrateRoot);
                }
                TokenKind::Ident { .. } => {
                    self.at += 1;
                    return Some(PathWord::Name(stream.text_at(at)));
                }
                _ => {
                    self.at += 1;
                    return Some(PathWord::NotAPath);
                }
            }
        }
        None
    }
}

/// Whether the tokens `a_at` of `a` and `b_at` of `b` are one path, word by word ([`PathWord`]):
/// spacing, and how a `$crate` is held, are not read; a token no path holds makes them unequal.
pub fn same_path(a: &TokenStream<'_>, a_at: Range<usize>, b: &TokenStream<'_>, b_at: Range<usize>) -> bool {
    let (mut x, mut y) = (path_words(a, a_at), path_words(b, b_at));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return true,
            (Some(PathWord::NotAPath), _) | (_, Some(PathWord::NotAPath)) => return false,
            (p, q) if p == q => {}
            _ => return false,
        }
    }
}
