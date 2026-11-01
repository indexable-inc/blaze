//! The parser's input from token trees, which are the tokens of a macro's input and output.
//!
//! One pass over the trees' columns ([`tt::TokenTreesView::for_each_token`]): no span is
//! unpacked and no leaf decoded. An identifier's kind comes from a table by its symbol
//! ([`Keywords`]), not from its text, and each context's edition is asked once a conversion.
//! A token costs a few loads and one 4-byte word of [`parser::Input`]; a caller parsing many
//! times (a macro's matcher, once a fragment) reuses one input with the `_into` functions.
//! [`tree_words`] converts a whole tree once, into the words it keeps, so that any view of it is
//! parsed from a slice of them with no conversion.

use std::sync::LazyLock;

use blaze_hash::HashMap;
use intern::{Symbol, sym};
use span::{Edition, SyntaxContext};
use syntax::{SyntaxKind, SyntaxKind::*, T};
use tt::FlatToken;

pub fn to_parser_input<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
) -> parser::Input {
    let mut res = parser::Input::with_capacity(buffer.len());
    convert::<false, F>(buffer, span_to_edition, &mut res);
    res
}

/// The input of a macro's matcher: the first token of each group without delimiters, which is
/// a fragment a macro transcribed, is marked as one, so that a matcher parses it whole, as rustc
/// does. `count` followed by `$n` of `0u8 + 1u8` is no call of `count` to `(0u8 + 1u8)`.
pub fn to_matcher_input<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
) -> parser::Input {
    let mut res = parser::Input::with_capacity(buffer.len());
    convert::<true, F>(buffer, span_to_edition, &mut res);
    res
}

/// [`to_parser_input`] into `res`, emptied first, keeping its buffer.
pub fn to_parser_input_into<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
    res: &mut parser::Input,
) {
    convert::<false, F>(buffer, span_to_edition, res)
}

/// [`to_matcher_input`] into `res`, emptied first, keeping its buffer.
pub fn to_matcher_input_into<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
    res: &mut parser::Input,
) {
    convert::<true, F>(buffer, span_to_edition, res)
}

/// The conversion, `FRAGMENTS` for a matcher's input: a parser's has no fragment to track.
fn convert<const FRAGMENTS: bool, F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    buffer: tt::TokenTreesView<'_>,
    span_to_edition: &mut F,
    res: &mut parser::Input,
) {
    res.clear();
    res.reserve(buffer.len());
    let mut conversion = Conversion::<FRAGMENTS, F>::new(span_to_edition, res, None);
    buffer.for_each_token(|token, ctx| conversion.token(token, ctx));
    conversion.finish();
}

/// The parser's words of `tree`'s tokens, as a matcher's input of the whole tree makes them,
/// with where each token slot's word is and the group of each fragment start: what
/// [`tt::Tree::words`] keeps, so that a view's input is a slice of them.
pub fn tree_words<F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    tree: &tt::Tree,
    span_to_edition: &mut F,
) -> tt::TreeWords {
    let mut res = parser::Input::with_capacity(tree.token_slots());
    // alloc: the index of the tree's words, kept with them in the tree.
    let mut at = vec![0; tree.token_slots()];
    // alloc: the tree's fragment starts, kept with its words; most trees have none.
    let mut fragment_starts = Vec::new();
    let slots = Slots { at: &mut at, fragment_starts: &mut fragment_starts, slot: 0, origin: 0 };
    let mut conversion = Conversion::<true, F>::new(span_to_edition, &mut res, Some(slots));
    tree.for_each_slotted_token(|slot, token, ctx| {
        conversion.at_slot(slot);
        conversion.token(token, ctx);
    });
    // alloc: the editions the words were made with, kept with them; a tree has a few contexts.
    let editions = conversion.finish().answers().collect();
    tt::TreeWords::new(
        tree,
        res.into_words().into_boxed_slice(),
        at.into_boxed_slice(),
        fragment_starts.into_boxed_slice(),
        editions,
    )
}

/// The parser's tokens of `view`, from the words its tree keeps ([`tree_words`], made the first
/// time): read in place when only the words' end padding follows the view's, as for a whole
/// tree, otherwise copied into `copy`. They are [`to_matcher_input`]'s tokens of the view, or with
/// `fragments` false [`to_parser_input`]'s, and nothing is converted.
///
/// The kept words hold the editions their contexts were given when they were made. When
/// `span_to_edition` gives any of those contexts another, the view alone is converted into
/// `copy` for this call, by the same conversion (the tree's words made again would cost the
/// tree, not the view), and the kept ones are left as they are.
///
/// A tree that keeps no words (a task's, [`tt::Tree::keeps_words`]) has its view converted into
/// `copy` on every call.
pub fn view_tokens<'a, F: FnMut(SyntaxContext) -> Edition + ?Sized>(
    view: tt::TokenTreesView<'a>,
    fragments: bool,
    span_to_edition: &mut F,
    copy: &'a mut parser::Input,
) -> parser::Tokens<'a> {
    if !view.tree().keeps_words() {
        match fragments {
            true => to_matcher_input_into(view, span_to_edition, copy),
            false => to_parser_input_into(view, span_to_edition, copy),
        }
        let copy: &'a parser::Input = copy;
        return copy.tokens();
    }
    let kept = view.tree_words(|tree| tree_words(tree, &mut *span_to_edition));
    let (tokens, first_fragment_counts) =
        match kept.editions().iter().all(|&(ctx, edition)| span_to_edition(ctx) == edition) {
            true => {
                let words = view.words(kept);
                let tokens = match words.rest.len() - words.len == parser::Tokens::PAD {
                    true => parser::Tokens::new(words.rest, true),
                    false => {
                        copy.clear();
                        copy.extend_from_words(words.words());
                        let copy: &'a parser::Input = copy;
                        copy.tokens()
                    }
                };
                (tokens, words.first_fragment_counts)
            }
            false => {
                match fragments {
                    true => to_matcher_input_into(view, span_to_edition, copy),
                    false => to_parser_input_into(view, span_to_edition, copy),
                }
                let copy: &'a parser::Input = copy;
                return copy.tokens();
            }
        };
    let tokens = if fragments { tokens } else { tokens.ignoring_fragments() };
    if first_fragment_counts { tokens } else { tokens.without_first_fragment() }
}

/// Where a whole tree's tokens' words are, recorded as [`tree_words`] converts it.
struct Slots<'c> {
    at: &'c mut [u32],
    fragment_starts: &'c mut Vec<(u32, u32)>,
    /// The slot of the token being converted.
    slot: u32,
    /// The slot of the invisible group that opened the fragment start waiting for a token.
    origin: u32,
}

/// One conversion's state, fed one token at a time.
struct Conversion<'c, 'f, const FRAGMENTS: bool, F: FnMut(SyntaxContext) -> Edition + ?Sized> {
    keywords: &'static Keywords,
    editions: Editions<'f, F>,
    res: &'c mut parser::Input,
    /// Where the fragment that was opened last starts, until its first token is pushed.
    fragment_at: Option<usize>,
    /// Whether the last token was the `'` of a lifetime, whose name comes next.
    lifetime: bool,
    slots: Option<Slots<'c>>,
}

impl<'c, 'f, const FRAGMENTS: bool, F: FnMut(SyntaxContext) -> Edition + ?Sized>
    Conversion<'c, 'f, FRAGMENTS, F>
{
    fn new(
        span_to_edition: &'f mut F,
        res: &'c mut parser::Input,
        slots: Option<Slots<'c>>,
    ) -> Self {
        Conversion {
            keywords: &*KEYWORDS,
            editions: Editions::new(span_to_edition),
            res,
            fragment_at: None,
            lifetime: false,
            slots,
        }
    }

    #[inline]
    fn at_slot(&mut self, slot: u32) {
        if let Some(slots) = &mut self.slots {
            slots.at[slot as usize] = self.res.len() as u32;
            slots.slot = slot;
        }
    }

    /// Marks the token at `at` as a fragment's first, if one is waiting for it.
    #[inline]
    fn start_fragment(&mut self) {
        if FRAGMENTS
            && let Some(at) = self.fragment_at
            && self.res.len() > at
        {
            self.res.mark_fragment_start(at);
            if let Some(slots) = &mut self.slots {
                slots.fragment_starts.push((at as u32, slots.origin));
            }
            self.fragment_at = None;
        }
    }

    fn token(&mut self, token: FlatToken, ctx: SyntaxContext) {
        self.start_fragment();
        let res = &mut *self.res;
        let editions = &mut self.editions;
        if self.lifetime {
            self.lifetime = false;
            match token {
                FlatToken::Ident { .. } => res.push(LIFETIME_IDENT, editions.of(ctx)),
                _ => panic!("Next token must be ident"),
            }
            return;
        }
        match token {
            FlatToken::Punct { char: '\'', .. } => self.lifetime = true,
            FlatToken::Punct { char, spacing } => {
                let kind = SyntaxKind::from_char(char)
                    .unwrap_or_else(|| panic!("{char:?} is not a valid punct"));
                res.push(kind, editions.of(ctx));
                if spacing == tt::Spacing::Joint {
                    res.was_joint();
                }
            }
            FlatToken::Ident { sym, is_raw } => {
                let edition = editions.of(ctx);
                if sym == sym::underscore {
                    res.push(T![_], edition);
                } else if is_raw.yes() {
                    let kind = if is_lifetime(sym.as_str()) { LIFETIME_IDENT } else { IDENT };
                    res.push(kind, edition);
                } else {
                    match self.keywords.of(sym, edition) {
                        (kind, EOF) => res.push(kind, edition),
                        (_, contextual) => res.push_ident(contextual, edition),
                    }
                }
            }
            FlatToken::Literal { sym, kind, suffix_len } => {
                let kind = literal_kind(kind);
                res.push(kind, editions.of(ctx));
                if kind == FLOAT_NUMBER {
                    let text = sym.as_str();
                    if !text[..text.len() - suffix_len as usize].ends_with('.') {
                        // Tag the token as joint if it is float with a fractional part
                        // we use this jointness to inform the parser about what token split
                        // event to emit when we encounter a float literal in a field access
                        res.was_joint();
                    }
                }
            }
            FlatToken::Open(delimiter) => match delimiter {
                tt::DelimiterKind::Parenthesis => res.push(T!['('], editions.of(ctx)),
                tt::DelimiterKind::Brace => res.push(T!['{'], editions.of(ctx)),
                tt::DelimiterKind::Bracket => res.push(T!['['], editions.of(ctx)),
                tt::DelimiterKind::Invisible => {
                    if FRAGMENTS {
                        // A waiting start is at `res.len()` too. Its origin is the innermost
                        // group: a view from any group opened before it holds that one too.
                        self.fragment_at = Some(res.len());
                        if let Some(slots) = &mut self.slots {
                            slots.origin = slots.slot;
                        }
                    }
                }
            },
            FlatToken::Close(delimiter) => match delimiter {
                tt::DelimiterKind::Parenthesis => res.push(T![')'], editions.of(ctx)),
                tt::DelimiterKind::Brace => res.push(T!['}'], editions.of(ctx)),
                tt::DelimiterKind::Bracket => res.push(T![']'], editions.of(ctx)),
                tt::DelimiterKind::Invisible => (),
            },
        }
    }

    /// Ends the conversion, giving back the editions it was told.
    fn finish(mut self) -> Editions<'f, F> {
        assert!(!self.lifetime, "Next token must be ident");
        self.start_fragment();
        self.editions
    }
}

#[inline]
fn literal_kind(kind: tt::LitKind) -> SyntaxKind {
    match kind {
        tt::LitKind::Byte => BYTE,
        tt::LitKind::Char => CHAR,
        tt::LitKind::Integer => INT_NUMBER,
        tt::LitKind::Float => FLOAT_NUMBER,
        tt::LitKind::Str | tt::LitKind::StrRaw(_) => STRING,
        tt::LitKind::ByteStr | tt::LitKind::ByteStrRaw(_) => BYTE_STRING,
        tt::LitKind::CStr | tt::LitKind::CStrRaw(_) => C_STRING,
        tt::LitKind::Err(_) => ERROR,
    }
}

/// How many contexts' editions a conversion keeps in place before it keeps them in a map; a
/// tree has few contexts, one an expansion it went through.
const NEAR: usize = 8;

/// Each context's edition, asked of the caller once: the last one, then up to [`NEAR`] in an
/// array, then a map. Nothing is allocated for a tree of [`NEAR`] contexts or fewer.
struct Editions<'f, F: FnMut(SyntaxContext) -> Edition + ?Sized> {
    span_to_edition: &'f mut F,
    last: (SyntaxContext, Edition),
    near: [(SyntaxContext, Edition); NEAR],
    len: usize,
    far: HashMap<SyntaxContext, Edition>,
}

impl<'f, F: FnMut(SyntaxContext) -> Edition + ?Sized> Editions<'f, F> {
    fn new(span_to_edition: &'f mut F) -> Self {
        let unused = (SyntaxContext::root(Edition::Edition2015), Edition::Edition2015);
        let far = HashMap::default();
        Editions { span_to_edition, last: unused, near: [unused; NEAR], len: 0, far }
    }

    #[inline]
    fn of(&mut self, ctx: SyntaxContext) -> Edition {
        if self.len > 0 && self.last.0 == ctx {
            return self.last.1;
        }
        self.asked(ctx)
    }

    fn asked(&mut self, ctx: SyntaxContext) -> Edition {
        let edition = match self.near[..self.len].iter().find(|it| it.0 == ctx) {
            Some(&(_, edition)) => edition,
            None => match self.far.get(&ctx) {
                Some(&edition) => edition,
                None => {
                    let edition = (self.span_to_edition)(ctx);
                    match self.len < NEAR {
                        true => {
                            self.near[self.len] = (ctx, edition);
                            self.len += 1;
                        }
                        false => {
                            self.far.insert(ctx, edition);
                        }
                    }
                    edition
                }
            },
        };
        self.last = (ctx, edition);
        edition
    }

    /// Each context asked about, with the edition it was given.
    fn answers(&self) -> impl Iterator<Item = (SyntaxContext, Edition)> {
        self.near[..self.len].iter().copied().chain(self.far.iter().map(|(&ctx, &e)| (ctx, e)))
    }
}

/// How many editions there are, indexed by their discriminants (`0..`, consecutive).
const EDITIONS: usize = Edition::LATEST as usize + 1;

/// What the parser is given for an identifier that is not raw, `_` or a lifetime, by edition:
/// a keyword's kind and `EOF`, or `IDENT` and its contextual kind (`IDENT` for none).
type IdentKinds = [(SyntaxKind, SyntaxKind); EDITIONS];

/// Whether an identifier's text is a lifetime's, which the parser takes as one.
#[inline]
fn is_lifetime(text: &str) -> bool {
    text.starts_with('\'')
}

/// [`IdentKinds`] for every keyword, found by its symbol: by [`Symbol::declared_index`] (a
/// pointer subtraction and two loads) for a keyword whose symbol is declared, else through a
/// map by symbol. Built once from [`SyntaxKind::from_keyword`] and
/// [`SyntaxKind::from_contextual_keyword`], so it answers as they do for every edition
/// (`keywords_answer_as_the_text_functions`). About 0.5 KB.
struct Keywords {
    /// By declared index: the keyword's place in `kinds` plus one, 0 for none.
    declared: Box<[u8]>,
    /// The keywords whose symbols are not declared ones, and their places plus one.
    other: HashMap<Symbol, u8>,
    /// Bit `n` set when some keyword in `other` is `n` bytes long: most identifiers are no
    /// keyword's length and skip the map.
    other_lengths: u64,
    kinds: Box<[IdentKinds]>,
}

static KEYWORDS: LazyLock<Keywords> = LazyLock::new(Keywords::new);

impl Keywords {
    fn new() -> Keywords {
        // alloc: built once, for the process.
        let mut texts: Vec<&'static str> = SyntaxKind::keyword_texts().collect();
        texts.sort_unstable();
        texts.dedup();
        // alloc: built once, for the process.
        let mut declared = Vec::new();
        let mut other = HashMap::default();
        let mut other_lengths = 0u64;
        let mut kinds = Vec::with_capacity(texts.len());
        for text in texts {
            kinds.push(std::array::from_fn(|edition| {
                ident_kinds(text, Edition::from_u32(edition as u32))
            }));
            let place = u8::try_from(kinds.len()).expect("fewer than 256 keywords");
            let symbol = Symbol::intern(text);
            match symbol.declared_index() {
                Some(at) => {
                    let at = usize::from(at);
                    if declared.len() <= at {
                        declared.resize(at + 1, 0);
                    }
                    declared[at] = place;
                }
                None => {
                    assert!(text.len() < 64, "a keyword of {} bytes", text.len());
                    other_lengths |= 1 << text.len();
                    other.insert(symbol, place);
                }
            }
        }
        Keywords { declared: declared.into(), other, other_lengths, kinds: kinds.into() }
    }

    /// What the parser is given for an identifier that is not raw or `_`: a keyword's kinds,
    /// `LIFETIME_IDENT` for a lifetime's text, else `IDENT`. Reads the text at most once and
    /// not at all for a declared keyword.
    #[inline]
    fn of(&self, symbol: Symbol, edition: Edition) -> (SyntaxKind, SyntaxKind) {
        let place = match symbol.declared_index() {
            Some(at) => self.declared.get(usize::from(at)).copied().unwrap_or(0),
            None => {
                let text = symbol.as_str();
                if self.other_lengths & 1 << text.len().min(63) == 0 {
                    return Self::not_keyword(text);
                }
                self.other.get(&symbol).copied().unwrap_or(0)
            }
        };
        match place {
            0 => Self::not_keyword(symbol.as_str()),
            place => self.kinds[usize::from(place) - 1][edition as usize],
        }
    }

    #[inline]
    fn not_keyword(text: &str) -> (SyntaxKind, SyntaxKind) {
        match is_lifetime(text) {
            true => (LIFETIME_IDENT, EOF),
            false => (IDENT, IDENT),
        }
    }
}

/// [`IdentKinds`] of `text` in `edition`, from the text.
fn ident_kinds(text: &str, edition: Edition) -> (SyntaxKind, SyntaxKind) {
    match SyntaxKind::from_keyword(text, edition) {
        Some(kind) => (kind, EOF),
        None => (IDENT, SyntaxKind::from_contextual_keyword(text, edition).unwrap_or(IDENT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every keyword, and identifiers that are none, get from the table what the text
    /// functions give, in every edition, declared symbol or not.
    #[test]
    fn keywords_answer_as_the_text_functions() {
        let others = [
            "foo",
            "Self_",
            "r",
            "unionx",
            "a",
            "macro_rule",
            "_x",
            "xxxxxxxxxxxxxxxxxxxx",
            "'a",
            "'static",
            "'_",
            "'abcdefgh",
        ];
        let texts = SyntaxKind::keyword_texts().chain(others.iter().copied());
        for text in texts {
            let symbol = Symbol::intern(text);
            for edition in Edition::iter() {
                let expected = match is_lifetime(text) {
                    true => (LIFETIME_IDENT, EOF),
                    false => ident_kinds(text, edition),
                };
                assert_eq!(KEYWORDS.of(symbol, edition), expected, "{text} in {edition:?}");
            }
        }
        for text in ["fn", "union", "dyn"] {
            for edition in Edition::iter() {
                assert_ne!(KEYWORDS.of(Symbol::intern(text), edition), (IDENT, IDENT), "{text}");
            }
        }
        // `async` is a keyword from 2018; in 2015 it is an identifier, as in rustc.
        for edition in Edition::iter() {
            let plain = KEYWORDS.of(Symbol::intern("async"), edition) == (IDENT, IDENT);
            assert_eq!(plain, edition == Edition::Edition2015, "async in {edition:?}");
        }
    }

    /// Asking a context's edition goes to the caller once per context.
    #[test]
    fn editions_are_asked_once_a_context() {
        let contexts: Vec<SyntaxContext> = Edition::iter().map(SyntaxContext::root).collect();
        let mut asked = Vec::new();
        let mut ask = |ctx: SyntaxContext| {
            asked.push(ctx);
            Edition::iter().find(|&it| SyntaxContext::root(it) == ctx).unwrap()
        };
        let mut editions = Editions::new(&mut ask);
        for _ in 0..3 {
            for (&ctx, edition) in contexts.iter().zip(Edition::iter()) {
                assert_eq!(editions.of(ctx), edition);
                assert_eq!(editions.of(ctx), edition);
            }
        }
        drop(editions);
        assert_eq!(asked, contexts);
    }
}
