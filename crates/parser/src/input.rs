//! See [`Input`].

use edition::Edition;

use crate::SyntaxKind;

/// Input for the parser -- a sequence of tokens.
///
/// As of now, parser doesn't have access to the *text* of the tokens, and makes
/// decisions based solely on their classification. Unlike `LexerToken`, the
/// `Tokens` doesn't include whitespace and comments. Main input to the parser.
///
/// One `u32` word a token, the lexer's word family ([`crate::LexedStr`], the flat tree): the
/// kind in the low [`KIND_BITS`] bits, then the contextual kind, the edition, whether the token
/// is joint to the next and whether it starts a fragment a macro transcribed. 4 bytes a token
/// against the 5.25 of five columns, and every question about a token is one load.
///
/// `words` ends in [`PAD`] words of an `EOF` token past the last real one, as every
/// [`Tokens`] does. Every word is made by [`word`] from `SyntaxKind`s or appended by
/// [`Input::extend_from_words`] with its kinds checked, and only the flag bits [`JOINT`] and
/// [`FRAGMENT`], above both kinds, are set after.
///
/// The words are the thread's [`InputWords`], back on its shelf when the input is dropped, so a
/// warm input allocates nothing.
pub struct Input {
    words: blaze_mem::scratch::Lent<InputWords>,
}

crate::event::per_thread_buffer! {
    /// An input's words: kept per thread at the most they ever held, back on the shelf of the
    /// thread that drops the input.
    InputWords, INPUT_WORDS: u32
}

/// The `EOF` words at the end of every [`Tokens`]: the parser looks at most 3 tokens past a
/// position no further than the end.
const PAD: usize = 4;

const KIND_BITS: u32 = 10;
const KIND_MASK: u32 = (1 << KIND_BITS) - 1;
const CONTEXTUAL_SHIFT: u32 = KIND_BITS;
const EDITION_SHIFT: u32 = 2 * KIND_BITS;
const EDITION_MASK: u32 = 0b111;
/// The bit of a word saying its token is joint to the next.
pub const JOINT: u32 = 1 << (EDITION_SHIFT + 3);
/// The bit of a word saying its token is the first of a fragment a macro transcribed.
pub const FRAGMENT: u32 = JOINT << 1;

const _: () = assert!((SyntaxKind::__LAST as u32) <= KIND_MASK, "a kind fits in KIND_BITS");
const _: () = assert!((Edition::LATEST as u32) <= EDITION_MASK, "an edition fits in 3 bits");

/// The word of a token of `kind` and `contextual_kind` in `edition`, neither joint nor a
/// fragment's start.
#[inline]
pub const fn word(kind: SyntaxKind, contextual_kind: SyntaxKind, edition: Edition) -> u32 {
    kind as u16 as u32
        | (contextual_kind as u16 as u32) << CONTEXTUAL_SHIFT
        | (edition as u32) << EDITION_SHIFT
}

/// The word of the `EOF` tokens past the end.
const EOF_WORD: u32 = word(SyntaxKind::EOF, SyntaxKind::EOF, Edition::LATEST);

/// Whether both kind fields of `word` are a `SyntaxKind`.
#[inline]
const fn has_valid_kinds(word: u32) -> bool {
    let last = SyntaxKind::__LAST as u32;
    (word & KIND_MASK) <= last && ((word >> CONTEXTUAL_SHIFT) & KIND_MASK) <= last
}

/// The kind in the low [`KIND_BITS`] of `bits`, a kind field of a word of a [`Tokens`].
///
/// Checked: one compare, never taken, as every word of a `Tokens` holds two kinds (made by
/// `word`, or checked by `has_valid_kinds` in `Input::extend_from_words` and `Tokens::new`);
/// an unchecked transmute here would save that compare and rest on all of those.
#[inline]
fn kind_of(bits: u32) -> SyntaxKind {
    SyntaxKind::from((bits & KIND_MASK) as u16)
}

/// `pub` impl used by callers to create `Tokens`.
impl Input {
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        let mut words = blaze_mem::scratch::lend::<InputWords>();
        words.reserve(capacity + PAD);
        words.extend([EOF_WORD; PAD]);
        Self { words }
    }

    /// The input's words, as [`Tokens::new`] reads them, copied out: the buffer goes back to
    /// the thread's shelf.
    #[inline]
    pub fn into_words(self) -> Vec<u32> {
        // alloc: the words a token tree keeps (`tree_words`), exactly as long as they are.
        self.words.to_vec()
    }

    /// Empties the input, keeping its buffer for the next one.
    #[inline]
    pub fn clear(&mut self) {
        self.words.truncate(PAD);
        self.words.fill(EOF_WORD);
    }

    /// Makes room for `additional` more tokens.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.words.reserve(additional);
    }

    /// Appends tokens by their words, as a [`Tokens`] holds them: a token tree's view that ends
    /// before its tree does, so is not followed by the pad. Panics when a word has a kind field
    /// that is no `SyntaxKind`, as [`Tokens::new`] does.
    #[inline]
    pub fn extend_from_words(&mut self, words: &[u32]) {
        assert!(
            words.iter().fold(true, |valid, &word| valid & has_valid_kinds(word)),
            "a parser word holds a kind that is no SyntaxKind"
        );
        let len = self.len();
        self.words.truncate(len);
        self.words.extend_from_slice(words);
        self.words.extend([EOF_WORD; PAD]);
    }

    /// Appends a token's word: it takes the place of the first pad word, and a pad word goes
    /// on the end.
    #[inline]
    fn push_word(&mut self, word: u32) {
        let at = self.len();
        self.words[at] = word;
        self.words.push(EOF_WORD);
    }

    #[inline]
    pub fn push(&mut self, kind: SyntaxKind, edition: Edition) {
        self.push_word(word(kind, SyntaxKind::EOF, edition));
    }
    #[inline]
    pub fn push_ident(&mut self, contextual_kind: SyntaxKind, edition: Edition) {
        self.push_word(word(SyntaxKind::IDENT, contextual_kind, edition));
    }
    /// Sets jointness for the last token we've pushed.
    ///
    /// This is a separate API rather than an argument to the `push` to make it
    /// convenient both for textual and mbe tokens. With text, you know whether
    /// the *previous* token was joint, with mbe, you know whether the *current*
    /// one is joint. This API allows for styles of usage:
    ///
    /// ```ignore
    /// // In text:
    /// tokens.was_joint(prev_joint);
    /// tokens.push(curr);
    ///
    /// // In MBE:
    /// token.push(curr);
    /// tokens.push(curr_joint)
    /// ```
    #[inline]
    pub fn was_joint(&mut self) {
        let last = self.len().checked_sub(1).expect("`was_joint` after a token");
        self.words[last] |= JOINT;
    }
    /// Marks token `n` as the first of a fragment that a macro transcribed (an `$e:expr`): as
    /// to rustc, the fragment is one operand, which nothing before it takes as its operator,
    /// call or index.
    #[inline]
    pub fn mark_fragment_start(&mut self, n: usize) {
        let len = self.len();
        assert!(n < len, "fragment start {n} of {len} tokens");
        self.words[n] |= FRAGMENT;
    }
}

/// pub(crate) impl used by the parser to consume `Tokens`.
impl Input {
    /// The input as the parser reads it, fragment starts included.
    #[inline]
    pub fn tokens(&self) -> Tokens<'_> {
        Tokens { words: &self.words, fragments: true, first_fragment: true }
    }
    #[inline]
    pub(crate) fn kind(&self, idx: usize) -> SyntaxKind {
        self.tokens().kind(idx)
    }
    #[inline]
    pub(crate) fn contextual_kind(&self, idx: usize) -> SyntaxKind {
        self.tokens().contextual_kind(idx)
    }
    #[inline]
    pub(crate) fn edition(&self, idx: usize) -> Edition {
        self.tokens().edition(idx)
    }
    #[inline]
    pub(crate) fn is_joint(&self, n: usize) -> bool {
        self.tokens().is_joint(n)
    }
    #[inline]
    pub(crate) fn is_fragment_start(&self, n: usize) -> bool {
        self.tokens().is_fragment_start(n)
    }
}

/// The parser's input where its words already are: an [`Input`]'s, or a token tree's, which
/// keeps the parser's word for each of its tokens, so that parsing it converts nothing. Each
/// word is as [`word`] makes it, with [`JOINT`] and [`FRAGMENT`]; with `fragments` false the
/// parser ignores [`FRAGMENT`], as it does in an expansion's output.
///
/// The words end in [`Tokens::PAD`] words [`Tokens::EOF_WORD`] past the last token, so that
/// `kind`, `contextual_kind` and `is_joint` up to 3 tokens past a position no further than the
/// end (the parser's reach) are a plain load with no `EOF` branch; past that they panic. Both
/// kind fields of every word are a `SyntaxKind`, so a kind is read with no range check.
/// `is_fragment_start` reads `false` past the end at any index, and `edition` panics there.
#[derive(Clone, Copy, Debug)]
pub struct Tokens<'t> {
    words: &'t [u32],
    fragments: bool,
    /// Whether a fragment start on token 0 counts ([`Tokens::without_first_fragment`]).
    first_fragment: bool,
}

impl<'t> Tokens<'t> {
    /// The `EOF` words that end the words of every `Tokens`.
    pub const PAD: usize = PAD;
    /// The word of each of them.
    pub const EOF_WORD: u32 = EOF_WORD;

    /// The tokens of `words`, which end in [`Tokens::PAD`] words [`Tokens::EOF_WORD`] that are
    /// no token. Panics when they don't, or when a word has a kind field that is no
    /// `SyntaxKind`: one pass over the words, a compare of each, at memory bandwidth.
    #[inline]
    pub fn new(words: &'t [u32], fragments: bool) -> Tokens<'t> {
        let tokens = words.len().checked_sub(PAD).expect("the parser's words end in 4 EOF words");
        assert!(
            words[tokens..].iter().all(|&word| word == EOF_WORD),
            "the parser's words end in 4 EOF words"
        );
        assert!(
            words.iter().fold(true, |valid, &word| valid & has_valid_kinds(word)),
            "a parser word holds a kind that is no SyntaxKind"
        );
        Tokens { words, fragments, first_fragment: true }
    }

    /// The same tokens, with a fragment start on token 0 not counted: a token tree's view that
    /// starts inside the invisible group whose opening made its first token a fragment start,
    /// which a conversion of the view alone would not mark.
    #[inline]
    pub fn without_first_fragment(self) -> Tokens<'t> {
        Tokens { first_fragment: false, ..self }
    }

    /// The same tokens, with no fragment start counted: an expansion's output, which the
    /// parser reads as one stream.
    #[inline]
    pub fn ignoring_fragments(self) -> Tokens<'t> {
        Tokens { fragments: false, ..self }
    }

    #[inline]
    pub fn len(self) -> usize {
        self.words.len() - PAD
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Token `i` as the parser sees it: its kind, contextual kind, edition, whether it is
    /// joint and whether it starts a fragment. For comparing inputs made two ways.
    pub fn token(self, i: usize) -> (SyntaxKind, SyntaxKind, Edition, bool, bool) {
        (
            self.kind(i),
            self.contextual_kind(i),
            self.edition(i),
            self.is_joint(i),
            self.is_fragment_start(i),
        )
    }

    #[inline]
    pub(crate) fn kind(self, idx: usize) -> SyntaxKind {
        kind_of(self.words[idx])
    }
    #[inline]
    pub(crate) fn contextual_kind(self, idx: usize) -> SyntaxKind {
        kind_of(self.words[idx] >> CONTEXTUAL_SHIFT)
    }
    #[inline]
    pub(crate) fn edition(self, idx: usize) -> Edition {
        Edition::from_u32((self.words[..self.len()][idx] >> EDITION_SHIFT) & EDITION_MASK)
    }
    #[inline]
    pub(crate) fn is_joint(self, n: usize) -> bool {
        self.words[n] & JOINT != 0
    }
    #[inline]
    pub(crate) fn is_fragment_start(self, n: usize) -> bool {
        self.fragments
            && (n != 0 || self.first_fragment)
            && self.words.get(n).is_some_and(|&word| word & FRAGMENT != 0)
    }
}

impl<'t> From<&'t Input> for Tokens<'t> {
    #[inline]
    fn from(input: &'t Input) -> Tokens<'t> {
        input.tokens()
    }
}

impl Input {
    #[inline]
    pub fn len(&self) -> usize {
        self.words.len() - PAD
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Token `i` as the parser sees it: its kind, contextual kind, edition, whether it is
    /// joint and whether it starts a fragment. For comparing inputs made two ways.
    pub fn token(&self, i: usize) -> (SyntaxKind, SyntaxKind, Edition, bool, bool) {
        self.tokens().token(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field reads back what was written, for every kind, edition and flag, and no
    /// field disturbs another.
    #[test]
    fn words_read_back_their_fields() {
        let mut input = Input::with_capacity(0);
        let mut expected = Vec::new();
        for raw in 0..=(SyntaxKind::__LAST as u16) {
            let kind = SyntaxKind::from(raw);
            for edition in Edition::iter() {
                let contextual = SyntaxKind::from(SyntaxKind::__LAST as u16 - raw);
                let i = input.len();
                let ident = raw % 3 == 0;
                if ident {
                    input.push_ident(contextual, edition);
                } else {
                    input.push(kind, edition);
                }
                let joint = raw % 2 == 0;
                if joint {
                    input.was_joint();
                }
                let fragment = raw % 5 == 0;
                if fragment {
                    input.mark_fragment_start(i);
                }
                expected.push(match ident {
                    true => (SyntaxKind::IDENT, contextual, edition, joint, fragment),
                    false => (kind, SyntaxKind::EOF, edition, joint, fragment),
                });
            }
        }
        for (i, expected) in expected.iter().enumerate() {
            assert_eq!(input.token(i), *expected, "token {i}");
        }
        let words: Vec<u32> = input.tokens().words.to_vec();
        let borrowed = Tokens::new(&words, true);
        let without_fragments = Tokens::new(&words, false);
        assert_eq!(borrowed.len(), input.len());
        for (i, &(kind, contextual, edition, joint, _)) in expected.iter().enumerate() {
            assert_eq!(borrowed.token(i), input.token(i), "token {i} borrowed");
            assert_eq!(
                without_fragments.token(i),
                (kind, contextual, edition, joint, false),
                "token {i} without fragments"
            );
        }
        assert_eq!(input.kind(input.len()), SyntaxKind::EOF);
        assert_eq!(input.contextual_kind(input.len() + PAD - 1), SyntaxKind::EOF);
        assert!(!input.is_joint(input.len()) && !input.is_fragment_start(input.len() + 7));
        input.clear();
        assert!(input.is_empty());
    }

    /// The unpadded words of `input`'s tokens, read as before the pad: through
    /// `SyntaxKind::from`'s range check, and `EOF` (not joint, no fragment) past the end.
    struct Unpadded(Vec<u32>);

    impl Unpadded {
        fn kind(&self, i: usize) -> SyntaxKind {
            self.0.get(i).map_or(SyntaxKind::EOF, |&w| SyntaxKind::from((w & KIND_MASK) as u16))
        }
        fn contextual_kind(&self, i: usize) -> SyntaxKind {
            self.0.get(i).map_or(SyntaxKind::EOF, |&w| {
                SyntaxKind::from(((w >> CONTEXTUAL_SHIFT) & KIND_MASK) as u16)
            })
        }
        fn is_joint(&self, i: usize) -> bool {
            self.0.get(i).is_some_and(|&w| w & JOINT != 0)
        }
        fn is_fragment_start(&self, i: usize) -> bool {
            self.0.get(i).is_some_and(|&w| w & FRAGMENT != 0)
        }
    }

    /// Inputs of every length from 0 to 40, of every kind in turn, joint and fragment starts
    /// mixed, filled twice (the second time after `clear`): the padded input reads the same
    /// kind, contextual kind, jointness and fragment start as the unpadded words at every
    /// position the parser reaches, up to 3 past the end, and has the same length.
    #[test]
    fn the_padded_input_reads_as_the_unpadded_words() {
        let mut input = Input::with_capacity(0);
        let last = SyntaxKind::__LAST as u16;
        for len in 0..=40usize {
            for round in 0..2 {
                input.clear();
                let mut unpadded = Vec::new();
                for i in 0..len {
                    let raw = ((i * 37 + len * 11 + round) % (last as usize + 1)) as u16;
                    let edition = Edition::iter().nth(i % Edition::iter().count()).unwrap();
                    let mut w = if i % 4 == 0 {
                        let contextual = SyntaxKind::from(last - raw);
                        input.push_ident(contextual, edition);
                        word(SyntaxKind::IDENT, contextual, edition)
                    } else {
                        input.push(SyntaxKind::from(raw), edition);
                        word(SyntaxKind::from(raw), SyntaxKind::EOF, edition)
                    };
                    if (i + round) % 3 == 0 {
                        input.was_joint();
                        w |= JOINT;
                    }
                    if (i + len) % 7 == 0 {
                        input.mark_fragment_start(i);
                        w |= FRAGMENT;
                    }
                    unpadded.push(w);
                }
                let unpadded = Unpadded(unpadded);
                assert_eq!(input.len(), unpadded.0.len());
                assert_eq!(input.is_empty(), unpadded.0.is_empty());
                assert_eq!(input.tokens().len(), len);
                for i in 0..len + PAD {
                    assert_eq!(input.kind(i), unpadded.kind(i), "kind {i} of {len}");
                    assert_eq!(
                        input.contextual_kind(i),
                        unpadded.contextual_kind(i),
                        "contextual kind {i} of {len}"
                    );
                    assert_eq!(input.is_joint(i), unpadded.is_joint(i), "joint {i} of {len}");
                }
                for i in 0..len + 2 * PAD {
                    assert_eq!(
                        input.is_fragment_start(i),
                        unpadded.is_fragment_start(i),
                        "fragment {i} of {len}"
                    );
                }
            }
        }
    }

    #[test]
    #[should_panic(expected = "the parser's words end in 4 EOF words")]
    fn tokens_without_the_pad_are_refused() {
        let words = [word(SyntaxKind::IDENT, SyntaxKind::EOF, Edition::CURRENT); 6];
        Tokens::new(&words, false);
    }

    #[test]
    #[should_panic(expected = "a parser word holds a kind that is no SyntaxKind")]
    fn a_word_of_no_kind_is_refused() {
        let mut words = vec![KIND_MASK];
        words.extend([EOF_WORD; PAD]);
        Tokens::new(&words, false);
    }

    #[test]
    #[should_panic]
    fn reading_past_the_pad_panics() {
        let input = Input::with_capacity(0);
        input.kind(PAD);
    }

    /// A warm input allocates nothing: its words are the thread's, back on the shelf since
    /// the input before.
    #[test]
    fn a_warm_input_allocates_nothing() {
        let text = "fn f(a: u32) -> u32 { a + 1 }\n".repeat(100);
        let lexed = crate::LexedStr::new(Edition::CURRENT, &text);
        drop(lexed.to_input(Edition::CURRENT));
        blaze_alloc_count::assert_at_most(0, "a warm input of a lexed text", || {
            drop(lexed.to_input(Edition::CURRENT))
        });
    }

    /// Refilling a cleared input as long as before allocates nothing.
    #[test]
    fn a_cleared_input_refills_without_allocating() {
        let mut input = Input::with_capacity(0);
        for _ in 0..1000 {
            input.push(SyntaxKind::IDENT, Edition::CURRENT);
        }
        blaze_alloc_count::assert_at_most(0, "refilling a cleared input", || {
            input.clear();
            for _ in 0..1000 {
                input.push(SyntaxKind::IDENT, Edition::CURRENT);
            }
        });
        assert_eq!(input.len(), 1000);
    }

    /// Words copied out of the middle of another input read as the same tokens there, padded
    /// after, and appending to a reused input allocates nothing once it has had the room.
    #[test]
    fn words_copied_from_a_slice_read_as_they_did() {
        let mut source = Input::with_capacity(0);
        for i in 0..50usize {
            let raw = (i % (SyntaxKind::__LAST as usize + 1)) as u16;
            source.push(SyntaxKind::from(raw), Edition::CURRENT);
            if i % 3 == 0 {
                source.was_joint();
            }
            if i % 5 == 0 {
                source.mark_fragment_start(i);
            }
        }
        let words = source.tokens().words;
        let mut copy = Input::with_capacity(0);
        for (start, end) in [(0, 0), (0, 50), (7, 19), (20, 21), (49, 50)] {
            copy.clear();
            copy.extend_from_words(&words[start..end]);
            assert_eq!(copy.len(), end - start);
            for i in 0..end - start {
                assert_eq!(copy.token(i), source.token(start + i), "token {i} of {start}..{end}");
            }
            for i in end - start..end - start + PAD {
                assert_eq!(copy.kind(i), SyntaxKind::EOF);
                assert!(!copy.is_joint(i) && !copy.is_fragment_start(i));
            }
        }
        blaze_alloc_count::assert_at_most(0, "copying a view's words into a reused input", || {
            copy.clear();
            copy.extend_from_words(&words[..50]);
        });
    }

    #[test]
    #[should_panic(expected = "a parser word holds a kind that is no SyntaxKind")]
    fn copied_words_of_no_kind_are_refused() {
        Input::with_capacity(0).extend_from_words(&[KIND_MASK]);
    }

    /// `without_first_fragment` drops a fragment start on token 0 and on no other.
    #[test]
    fn without_first_fragment_drops_only_token_0s() {
        let mut input = Input::with_capacity(0);
        for i in 0..3 {
            input.push(SyntaxKind::IDENT, Edition::CURRENT);
            input.mark_fragment_start(i);
        }
        let tokens = input.tokens();
        let without = tokens.without_first_fragment();
        assert!(tokens.is_fragment_start(0) && !without.is_fragment_start(0));
        assert!(without.is_fragment_start(1) && without.is_fragment_start(2));
        assert_eq!(without.kind(0), tokens.kind(0));
    }
}
