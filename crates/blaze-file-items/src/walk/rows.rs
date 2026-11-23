//! The outline written as the grammar closes each item: the item grammar's sink
//! (`parser::Sink`), with no list of events, no `parser::Output` and no walk of one.
//!
//! The sink keeps a stack of open nodes ([`Frame`]) and, for each, the finished nodes directly
//! in it ([`Node`]: a kind and a range of tokens). A token costs it nothing: every position is
//! a token index into the columns ([`Columns`]: the parser's words, `starts`, `ends`, filled in
//! the one pass over the one lex of the whole text), and the flags machine
//! (crate::flags) is fed in that same pass, the few tokens that set a flag kept beside the
//! columns until the entry that holds them closes ([`Rows::take_flags`]). A node whose
//! parent is a list (`Sink::start_list`: the file, or an entry's body) is an entry: its row is
//! put down when it opens, so rows are in source order, and filled when it closes, from the
//! nodes directly in it and the tokens between them ([`Rows::finish`]): the rows a reader of
//! the finished tree would write. What the grammar may still do to a finished
//! node is kept exact: a node wrapped after the fact (`precede`) takes its place in its
//! parent's list, an abandoned node's children are its parent's, and the children of an
//! abandoned entry are entries themselves, made from the nodes each kept below it.

use parser::{Done, JOINT, Joiner, Message, Sink, Tokens};

use super::*;

/// No row, no token, no finished node.
const NONE: u32 = u32::MAX;

/// A finished node in its parent's list: tokens `start..end`.
///
/// Every node keeps everything below it until the entry around it closes, so a node that
/// becomes an entry when the item around it is given up ([`Rows::abandon_entry`]) is read from
/// its own children whatever the grammar did to it before (wrapped it, extended it, gave up
/// the node it was in): no form of a text is refused.
#[derive(Clone, Copy)]
struct Node {
    kind: SyntaxKind,
    /// A node of an item's kind ([`is_item`]) is strictly inside it.
    holds: bool,
    start: u32,
    end: u32,
    /// How many nodes right below it in the list are below it in the tree too: its children,
    /// each above its own. A list keeps none: its entries are rows already.
    kept: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Plain,
    /// A node directly in a list: its row is `Frame::row`.
    Entry,
    /// The file's root or an entry's body: the nodes directly in it are entries.
    List,
    /// An inner attribute before a list's first entry: the list's owner's.
    InnerAttr,
}

/// An open node.
#[derive(Clone, Copy)]
struct Frame {
    shape: Shape,
    /// A node of an item's kind is strictly inside it, so far.
    holds: bool,
    /// The next token when it opened.
    start: u32,
    /// Where its children start in [`Stacks::nodes`].
    base: u32,
    /// An entry's row; a list's owner's, or [`NONE`] for the file.
    row: u32,
    /// A list: the entries opened in it.
    count: u32,
    /// An entry: its attributes.
    attrs: AttrSpan,
    /// A list's kind.
    list: SyntaxKind,
    /// An entry: the token its body list opened at, or [`NONE`].
    list_at: u32,
}

/// Why tokens handed over with their text are not outlined ([`TokenColumns::push`],
/// `outline_tokens`): the token, counted from 0, and the rule it breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokensRefused {
    /// A token holds no byte: `end` is not after `start`.
    Empty { token: u32, start: u32, end: u32 },
    /// A token starts before the one given before it ends: tokens are given in the order of
    /// the text and share no byte.
    Overlaps { token: u32, start: u32, before_end: u32 },
    /// A word that is no token's (`parser::token_word`).
    Word { token: u32, word: u32, why: parser::WordRefused },
    /// A word of trivia or of the end (`u16::from(SyntaxKind)`): whitespace and comments are
    /// the bytes between tokens, and the end is where the tokens stop.
    NotAToken { token: u32, syntax: u16 },
    /// The text is longer than `u32::MAX` bytes: an outline's ranges are `u32` pairs.
    TextTooLong { len: usize },
    /// A token ends past the text's end.
    OutsideText { token: u32, end: u32, len: u32 },
    /// A token starts or ends inside a character of the text.
    SplitsChar { token: u32, at: u32 },
}

impl core::fmt::Display for TokensRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            TokensRefused::Empty { token, start, end } => {
                write!(f, "token {token} is bytes {start}..{end} of its text: a token holds at least one byte")
            }
            TokensRefused::Overlaps { token, start, before_end } => write!(
                f,
                "token {token} starts at byte {start}, before the token given before it ends ({before_end}): tokens are given in the text's order and share no byte"
            ),
            TokensRefused::Word { token, word, why } => write!(f, "token {token}'s word {word:#x} is no token's word: {why:?}"),
            TokensRefused::NotAToken { token, syntax } => write!(
                f,
                "token {token} is of syntax kind {syntax}, trivia or the end: whitespace and comments are the bytes between tokens"
            ),
            TokensRefused::TextTooLong { len } => write!(f, "a text of {len} bytes: an outline's ranges are u32 pairs"),
            TokensRefused::OutsideText { token, end, len } => write!(f, "token {token} ends at byte {end} of a text of {len} bytes"),
            TokensRefused::SplitsChar { token, at } => write!(f, "token {token} is cut at byte {at}, inside a character of its text"),
        }
    }
}

impl core::error::Error for TokensRefused {}

/// The tokens of a text its writer already has, in three columns: for a text that was written
/// from tokens (an expansion rendered), so that reading it back lexes nothing. The writer puts
/// each token's bytes in the text and gives [`TokenColumns::push`] its word and where it is;
/// `outline_tokens` reads the text for everything else (a name, an attribute's argument, a
/// digest's bytes).
///
/// Which tokens are joint is not the writer's to say: it is read off the text, a token being
/// joint to the next by `parser::Joiner`'s rule when no byte lies between them, as the lexer's
/// tokens of the same text are. So the bytes between two tokens must be what the lexer takes for
/// trivia there (whitespace, a comment that is no doc comment) and the bytes of each token what
/// it takes for that one token: `outline_tokens(text, tokens)` is then `outline(text)` in every
/// row, attribute, flag and digest (examples/oracle.rs `--tokens`, over both corpora).
pub struct TokenColumns<'a> {
    words: AVec<'a, u32>,
    starts: AVec<'a, u32>,
    ends: AVec<'a, u32>,
}

impl<'a> TokenColumns<'a> {
    /// No token: nothing is allocated until the first.
    pub fn new_in(alloc: Alloc<'a>) -> TokenColumns<'a> {
        TokenColumns { words: Vec::new_in(alloc), starts: Vec::new_in(alloc), ends: Vec::new_in(alloc) }
    }

    /// No token again, for the next text: what was allocated is kept.
    pub fn clear(&mut self) {
        self.words.clear();
        self.starts.clear();
        self.ends.clear();
    }

    /// How many tokens were given.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether no token was given.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// The next token: bytes `start..end` of the text, and its word (`parser::word` of its kind,
    /// an identifier's contextual keyword and its edition). Of a word kept from elsewhere (a
    /// token tree's) only those three are read (`parser::token_word`): `JOINT` is the text's to
    /// say and an outline has no fragment starts.
    ///
    /// Refused, with nothing added: a token of no byte, one that starts before the last one
    /// ends, a word that is no token's.
    #[inline]
    pub fn push(&mut self, word: u32, start: u32, end: u32) -> Result<(), TokensRefused> {
        let token = narrow(self.words.len());
        if end <= start {
            return Err(TokensRefused::Empty { token, start, end });
        }
        if let Some(&before_end) = self.ends.last()
            && start < before_end
        {
            return Err(TokensRefused::Overlaps { token, start, before_end });
        }
        let word = match parser::token_word(word) {
            Ok(word) => word,
            Err(why) => return Err(TokensRefused::Word { token, word, why }),
        };
        let kind = parser::word_kind(word);
        if kind.is_trivia() || kind == EOF {
            return Err(TokensRefused::NotAToken { token, syntax: u16::from(kind) });
        }
        self.words.push(word);
        self.starts.push(start);
        self.ends.push(end);
        Ok(())
    }
}

/// What the grammar and the sink read of a text's tokens that are not trivia, filled in one
/// pass over them: the parser's word of each with its joint mark, which the grammar reads in
/// place (`Tokens::new`: no `Input` is made); the tokens that set a flag; and, when the tokens
/// are the lexer's ([`Columns::fill`]), where each starts and ends in the text. Tokens handed
/// over ([`Columns::mark`]) bring those two columns themselves. One set per worker, cleared
/// for each text.
pub(super) struct Columns<'a> {
    /// The words, then the parser's pad of `EOF` words.
    words: AVec<'a, u32>,
    starts: AVec<'a, u32>,
    ends: AVec<'a, u32>,
    /// The tokens that set a flag, in order, each with the flags it sets: the flags machine
    /// (crate::flags) is fed in the same pass, while a token's kind and text are at hand, and
    /// the few tokens it answers for are all that is kept of it.
    flags: AVec<'a, (u32, ItemFlags)>,
}

/// One token into the words and the flags: its word, its own joint mark in it; `joins`, the
/// token before it is joint to it (`parser::Joins::before`). The one place a token of either
/// source goes through.
#[inline]
fn put(words: &mut AVec<'_, u32>, flags: &mut AVec<'_, (u32, ItemFlags)>, scan: &mut Scan, text: &str, word: u32, bytes: core::ops::Range<usize>, joins: bool) {
    if joins && let Some(last) = words.last_mut() {
        *last |= JOINT;
    }
    let set = scan.token(parser::word_kind(word), || &text[bytes]);
    if !set.is_empty() {
        flags.push((narrow(words.len()), set));
    }
    words.push(word);
}

impl<'a> Columns<'a> {
    pub(super) fn new_in(alloc: Alloc<'a>) -> Columns<'a> {
        Columns {
            words: Vec::new_in(alloc),
            starts: Vec::new_in(alloc),
            ends: Vec::new_in(alloc),
            flags: Vec::new_in(alloc),
        }
    }

    fn clear(&mut self) {
        self.words.clear();
        self.starts.clear();
        self.ends.clear();
        self.flags.clear();
    }

    /// The tokens the columns have room for: the measure of what the worker's set holds.
    pub(super) fn held(&self) -> usize {
        self.words.capacity()
    }

    /// Fills the columns with the tokens of `lexed`.
    fn fill(&mut self, lexed: &LexedStr<'_>, edition: Edition) {
        self.clear();
        let Columns { words, starts, ends, flags } = self;
        let text = lexed.as_str();
        let mut scan = Scan::new();
        let mut joiner = Joiner::new();
        lexed.each_input_token(edition, &mut joiner, |word, bytes, joins| {
            // A text's offsets fit `u32`: `outline` refuses a longer one before it lexes.
            starts.push(bytes.start as u32);
            ends.push(bytes.end as u32);
            put(words, flags, &mut scan, text, word, bytes, joins);
        });
        words.extend_from_slice(&[Tokens::EOF_WORD; Tokens::PAD]);
    }

    /// Fills the words and the flags from `tokens`, the tokens of `text` its writer handed
    /// over: each word marked joint by where the token stands in the text
    /// (`parser::Joiner`, a gap wherever a token does not start where the one before it ends).
    /// The starts and ends stay the caller's.
    fn mark(&mut self, text: &str, tokens: &TokenColumns<'_>) -> Result<(), TokensRefused> {
        self.clear();
        let Columns { words, flags, .. } = self;
        let Ok(len) = u32::try_from(text.len()) else {
            return Err(TokensRefused::TextTooLong { len: text.len() });
        };
        let mut scan = Scan::new();
        let mut joiner = Joiner::new();
        let mut before = 0;
        for (at, ((&word, &start), &end)) in tokens.words.iter().zip(tokens.starts.iter()).zip(tokens.ends.iter()).enumerate() {
            if end > len {
                return Err(TokensRefused::OutsideText { token: narrow(at), end, len });
            }
            for cut in [start, end] {
                if !text.is_char_boundary(cut as usize) {
                    return Err(TokensRefused::SplitsChar { token: narrow(at), at: cut });
                }
            }
            if start != before {
                joiner.gap();
            }
            before = end;
            let bytes = start as usize..end as usize;
            // A token is never empty (`TokenColumns::push`), and a UTF-8 continuation byte is
            // never `.`.
            let joins = joiner.token(parser::word_kind(word), || text.as_bytes()[bytes.end - 1] == b'.');
            put(words, flags, &mut scan, text, joins.marked(word), bytes, joins.before);
        }
        words.extend_from_slice(&[Tokens::EOF_WORD; Tokens::PAD]);
        Ok(())
    }
}

/// The sink's working vectors, one set per worker, cleared for each text.
pub(super) struct Stacks<'a> {
    frames: AVec<'a, Frame>,
    nodes: AVec<'a, Node>,
    /// The tokens that set a flag, in order, not yet given to an entry.
    flagged: AVec<'a, (u32, ItemFlags)>,
    tokens: AVec<'a, Tok>,
    order: AVec<'a, u32>,
    /// The words of a body parsed for what it holds, with their pad.
    body: AVec<'a, u32>,
}

impl<'a> Stacks<'a> {
    pub(super) fn new_in(alloc: Alloc<'a>) -> Stacks<'a> {
        Stacks {
            frames: Vec::new_in(alloc),
            nodes: Vec::new_in(alloc),
            flagged: Vec::new_in(alloc),
            tokens: Vec::new_in(alloc),
            order: Vec::new_in(alloc),
            body: Vec::new_in(alloc),
        }
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.nodes.clear();
        self.flagged.clear();
        self.tokens.clear();
        self.order.clear();
        self.body.clear();
    }
}

/// The outline of `text`: lexed once, whole; its tokens put in the parser's columns in one
/// pass; the item grammar run over them with the sink that writes the rows; then the digests
/// (crate::digest). No text is copied and no second reader finds a body: a `fn` body is the
/// block the grammar leaves unparsed (`parser::lazy_block_len`), decided from its tokens in the
/// columns ([`Sink::lazy`]).
///
/// With [`Bodies::Parsed`] the grammar parses every body with the rest: the same rows, and the
/// findings inside the bodies too.
pub(crate) fn outline<'a, C: Compress, F: Feed>(
    text: &str,
    edition: Edition,
    bodies: Bodies,
    alloc: Alloc<'a>,
) -> Outline<'a> {
    outline_with_extensions::<C, F>(text, edition, bodies, parser::Extensions::NONE, alloc)
}

/// [`outline`] with explicit blaze syntax extensions ([`parser::Extensions`]) enabled on the
/// parser input.
pub(crate) fn outline_with_extensions<'a, C: Compress, F: Feed>(
    text: &str,
    edition: Edition,
    bodies: Bodies,
    extensions: parser::Extensions,
    alloc: Alloc<'a>,
) -> Outline<'a> {
    assert!(
        u32::try_from(text.len()).is_ok(),
        "a source file of {} bytes: an outline's ranges are u32 pairs",
        text.len()
    );
    let mut buffers = blaze_mem::scratch::lend::<Buffers>();
    let Buffers { out, working } = &mut *buffers;
    let lexed = LexedStr::new(edition, text);
    working.columns.fill(&lexed, edition);
    let Columns { words, starts, ends, flags } = &working.columns;
    out.extensions = extensions;
    sink_rows(Seen { text, words, starts, ends, flags }, &mut working.rows, out, |tokens, rows| {
        parser::source_file_into(tokens.with_extensions(extensions), bodies == Bodies::Skipped, rows)
    });
    lexer_errors(&lexed, out);
    out.flags = out.items.iter().fold(out.own_flags, |flags, item| flags | item.flags);
    // The digests read the same columns: the one lex's tokens, no second pass over the text.
    let tokens = Lexed { words, starts, ends };
    digest::<C, F>(text, edition_byte(edition), tokens, out, &mut working.digest);
    out.copy_in(alloc)
}

/// The outline of `text` from `tokens`, its tokens as its writer handed them over
/// ([`TokenColumns`]): [`outline`] with no lexer run. The grammar, the sink and the digests are
/// the same and read the same columns; only who filled them differs, and the one rule for
/// which tokens are joint (`parser::Joiner`) is fed from where the tokens stand in the text.
/// `fn` bodies are left unparsed where the grammar can ([`Bodies::Skipped`]).
///
/// It holds no finding of the lexer's: nothing was lexed, and tokens that came from a source
/// text had their lexer's findings reported where that text was read.
pub(crate) fn outline_tokens<'a, C: Compress, F: Feed>(
    text: &str,
    tokens: &TokenColumns<'_>,
    edition: Edition,
    alloc: Alloc<'a>,
) -> Result<Outline<'a>, TokensRefused> {
    let mut buffers = blaze_mem::scratch::lend::<Buffers>();
    let Buffers { out, working } = &mut *buffers;
    working.columns.mark(text, tokens)?;
    let Columns { words, flags, .. } = &working.columns;
    let seen = Seen { text, words, starts: &tokens.starts, ends: &tokens.ends, flags };
    sink_rows(seen, &mut working.rows, out, |tokens, rows| parser::source_file_into(tokens, true, rows));
    out.flags = out.items.iter().fold(out.own_flags, |flags, item| flags | item.flags);
    let lexed = Lexed { words, starts: &tokens.starts, ends: &tokens.ends };
    digest::<C, F>(text, edition_byte(edition), lexed, out, &mut working.digest);
    Ok(out.copy_in(alloc))
}

fn narrow(at: usize) -> u32 {
    u32::try_from(at).expect("an outline counts in u32")
}

/// The lexer's errors of a text onto its outline, after the parser's.
fn lexer_errors(lexed: &LexedStr<'_>, out: &mut Outline<'_>) {
    for (token, message) in lexed.errors() {
        let at = lexed.text_range(token);
        let range = Range::new(narrow(at.start), narrow(at.end));
        push_error(&mut out.errors, &mut out.messages, range, ErrorOrigin::Lexer, message);
    }
}

/// A text and its tokens as the grammar and the sink read them: the words with their joint
/// marks and pad, where each token starts and ends, and the tokens that set a flag
/// ([`Columns`]).
#[derive(Clone, Copy)]
struct Seen<'t> {
    text: &'t str,
    words: &'t [u32],
    starts: &'t [u32],
    ends: &'t [u32],
    flags: &'t [(u32, ItemFlags)],
}

/// The rows, attributes and parser errors `drive` has the sink write of a text and its
/// tokens: `drive` is the item grammar over the tokens, given the sink and giving it back with
/// every node it opened closed.
fn sink_rows<'a>(
    seen: Seen<'_>,
    stacks: &mut Stacks<'a>,
    out: &mut Outline<'a>,
    drive: impl for<'t, 'o> FnOnce(Tokens<'t>, Rows<'t, 'o, 'a>) -> Rows<'t, 'o, 'a>,
) {
    let Seen { text, words, starts, ends, flags } = seen;
    assert_eq!(
        (starts.len() + Tokens::PAD, ends.len() + Tokens::PAD),
        (words.len(), words.len()),
        "a token has a word, a start and an end"
    );
    let tokens = Tokens::new(words, true);
    stacks.clear();
    let rows = Rows { text, words, tokens, starts, ends, flags, scanned: 0, s: stacks, out };
    let mut rows = drive(tokens, rows);
    assert!(rows.s.frames.is_empty(), "the grammar closes every node it opens");
    rows.scan_to(narrow(tokens.len()));
    for &(_, flags) in rows.s.flagged.iter() {
        rows.out.own_flags |= flags;
    }
}

/// Whether a block's parse makes a node of an item's kind: the sink of a body parsed for
/// `body_holds_items` alone. The answer only ever goes from false to true, and once it is true
/// nothing more of the parse is read, so the grammar is told to stop at the next statement
/// boundary ([`Sink::satisfied`]): a body costs its statements up to the first that holds an
/// item or a macro call, not all of them.
struct Holds(bool);

impl Sink for Holds {
    #[inline]
    fn satisfied(&self) -> bool {
        self.0
    }
    fn start(&mut self, _at: u32) -> u32 {
        0
    }
    fn token(&mut self, _kind: SyntaxKind, _n_raw_tokens: u8) {}
    fn complete(&mut self, _mark: u32, kind: SyntaxKind, at: u32) -> Done {
        self.0 |= is_item(kind);
        Done { start: 0, end: at }
    }
    fn abandon(&mut self, _mark: u32, _at: u32) {}
    fn precede(&mut self, _done: u32, _at: u32) -> u32 {
        0
    }
    fn extend_to(&mut self, done: u32, _mark: u32) -> u32 {
        done
    }
    fn split_float(&mut self, mark: u32, _ends_in_dot: bool, _at: u32) -> u32 {
        mark
    }
    fn error(&mut self, _message: Message, _at: u32) {}
    fn lazy(&mut self, kind: SyntaxKind, _n_input_tokens: u32, _at: u32) {
        unreachable!("a full parse leaves no lazy {kind:?}")
    }
    fn last_token(&self, end: u32, input: Tokens<'_>) -> Option<SyntaxKind> {
        end.checked_sub(1).map(|at| input.token(at as usize).0)
    }
}

/// What [`Rows::finish`] reads off an entry's direct children and tokens.
#[derive(Default)]
struct Facts {
    head_start: Option<u32>,
    before_body: Option<u32>,
    before_eq: Option<u32>,
    before_semi: Option<u32>,
    after_colon: bool,
    after_eq: bool,
    name: Option<Range>,
    visibility: Option<WrittenVis<Range>>,
    rename: Option<Rename<Range>>,
    tree: Option<Range>,
    path: Option<Range>,
    input: Option<Range>,
    abi: Option<Range>,
    ty: Option<Range>,
    value: Option<Range>,
    body: Option<Range>,
    /// The token the body starts at, when it is a list.
    body_list_at: u32,
    holds_items: bool,
}

struct Rows<'t, 'o, 'a> {
    text: &'t str,
    /// The token columns ([`Columns`]): the words with their pad, the same as `tokens`, and
    /// each token's start and end in `text`.
    words: &'t [u32],
    tokens: Tokens<'t>,
    starts: &'t [u32],
    ends: &'t [u32],
    /// The tokens that set a flag ([`Columns`]), and how many of them are already with the
    /// sink (`Stacks::flagged`, or an entry).
    flags: &'t [(u32, ItemFlags)],
    scanned: usize,
    s: &'o mut Stacks<'a>,
    out: &'o mut Outline<'a>,
}

impl Rows<'_, '_, '_> {
    /// The kind of token `at`, when there is one.
    #[inline]
    fn kind(&self, at: usize) -> Option<SyntaxKind> {
        (at < self.starts.len()).then(|| self.tokens.kind(at))
    }

    /// Where the walk stands before token `at`: the end of the token before it, or the text's
    /// start.
    fn offset_before(&self, at: u32) -> u32 {
        match at.checked_sub(1) {
            Some(before) => self.ends[before as usize],
            None => 0,
        }
    }

    /// The bytes of tokens `start..end`, or the empty range where the walk stands.
    fn range(&self, start: u32, end: u32) -> Range {
        if end > start {
            Range::new(self.starts[start as usize], self.ends[end as usize - 1])
        } else {
            let at = self.offset_before(start);
            Range::new(at, at)
        }
    }

    /// Moves the flags set by the tokens before `upto` to those not yet given to an entry.
    fn scan_to(&mut self, upto: u32) {
        while let Some(&(token, set)) = self.flags.get(self.scanned)
            && token < upto
        {
            self.s.flagged.push((token, set));
            self.scanned += 1;
        }
    }

    /// Takes the flags set by tokens `start..end`, leaving the others where they are.
    fn take_flags(&mut self, start: u32, end: u32) -> ItemFlags {
        let flagged = &mut self.s.flagged;
        let mut from = flagged.len();
        while from > 0 && flagged[from - 1].0 >= start {
            from -= 1;
        }
        let mut flags = ItemFlags::NONE;
        let mut kept = from;
        for at in from..flagged.len() {
            let (token, set) = flagged[at];
            if token < end {
                flags |= set;
            } else {
                flagged[kept] = (token, set);
                kept += 1;
            }
        }
        flagged.truncate(kept);
        flags
    }

    fn pop(&mut self, mark: u32) -> Frame {
        assert_eq!(mark as usize + 1, self.s.frames.len(), "the grammar closes its innermost open node");
        self.s.frames.pop().expect("an open node")
    }

    fn record(&mut self, tokens: (u32, u32)) {
        self.s.tokens.clear();
        for at in tokens.0 as usize..tokens.1 as usize {
            let range = Range::new(self.starts[at], self.ends[at]);
            self.s.tokens.push(Tok { kind: self.tokens.kind(at), range });
        }
    }

    /// An attribute or doc comment over tokens `start..end` of the owner starting at byte
    /// `base`, onto the outline's attributes: their new count.
    fn push_attr(&mut self, kind: SyntaxKind, start: u32, end: u32, base: u32) -> u32 {
        self.record((start, end));
        let (inner, attr) = attr_of(kind, &self.s.tokens, self.text);
        let attr = match attr {
            AttrKind::Path { literal } => AttrKind::Path { literal: relative(literal, base) },
            other => other,
        };
        let range = self.range(start, end);
        self.out.attrs.push(Attr { range: relative(range, base), inner, kind: attr });
        narrow(self.out.attrs.len())
    }

    /// The direct children of a node whose children are `nodes[lo..hi]`, onto `order`, last
    /// first: each is below the ones it kept.
    fn children(&mut self, lo: u32, hi: u32) -> usize {
        let from = self.s.order.len();
        let mut at = hi;
        while at > lo {
            let node = at - 1;
            self.s.order.push(node);
            at = node - self.s.nodes[node as usize].kept;
        }
        from
    }

    /// Fills `row`: the entry of `syntax` over tokens `start..end`, its children
    /// `nodes[lo..hi]`, each child and each token directly in it read in order.
    #[expect(clippy::too_many_arguments, reason = "an entry's node, its children and its row")]
    fn finish(&mut self, syntax: SyntaxKind, start: u32, end: u32, lo: u32, hi: u32, row: u32, mut attrs: AttrSpan, list_at: u32, eager: bool) {
        let node = self.range(start, end);
        let entry_start = node.start;
        let mut o = Facts { body_list_at: NONE, ..Facts::default() };
        let from = self.children(lo, hi);
        let mut at = start;
        for child in (from..self.s.order.len()).rev() {
            let c = self.s.nodes[self.s.order[child] as usize];
            self.gap(&mut o, at, c.start, start);
            at = c.end.max(at);
            let range = self.range(c.start, c.end);
            let head = (c.end > c.start).then(|| self.starts[c.start as usize]);
            match c.kind {
                ATTR | DOC_COMMENT => {
                    if !eager {
                        attrs.end = self.push_attr(c.kind, c.start, c.end, entry_start);
                    }
                    continue;
                }
                VISIBILITY | RENAME => {
                    self.record((c.start, c.end));
                    match c.kind {
                        VISIBILITY => o.visibility = Some(visibility_of(&self.s.tokens)),
                        _ => o.rename = rename_of(&self.s.tokens),
                    }
                    o.head_start = o.head_start.or(head);
                    continue;
                }
                _ => {}
            }
            let next = self.kind(c.start as usize);
            let starts_body = o.body.is_none()
                && match syntax {
                    FN => c.kind == BLOCK_EXPR,
                    MODULE => c.kind == ITEM_LIST,
                    TRAIT | IMPL => c.kind == ASSOC_ITEM_LIST,
                    EXTERN_BLOCK => c.kind == EXTERN_ITEM_LIST,
                    ENUM => c.kind == VARIANT_LIST,
                    STRUCT | UNION | VARIANT => matches!(c.kind, RECORD_FIELD_LIST | TUPLE_FIELD_LIST),
                    MACRO_RULES | MACRO_CALL => c.kind == TOKEN_TREE,
                    MACRO_DEF => c.kind == TOKEN_TREE && next == Some(L_CURLY),
                    CONST | STATIC => o.after_eq,
                    _ => false,
                };
            if starts_body {
                o.before_body = Some(match c.start > start {
                    true => self.ends[c.start as usize - 1],
                    false => self.offset_before(c.start),
                });
                o.body = Some(range);
                if is_list(c.kind) {
                    o.body_list_at = c.start;
                }
                if syntax == MACRO_CALL {
                    o.input = Some(range);
                }
                o.holds_items = c.holds && matches!(syntax, FN | CONST | STATIC);
                continue;
            }
            o.head_start = o.head_start.or(head);
            match (syntax, c.kind) {
                (_, child) if is_name_child(syntax, child) && c.start < c.end => {
                    o.name.get_or_insert(range);
                }
                (USE, USE_TREE) => o.tree = Some(range),
                (MACRO_CALL, PATH) => o.path = Some(range),
                (EXTERN_BLOCK, ABI) => o.abi = Some(range),
                (RECORD_FIELD, _) if o.after_colon && o.ty.is_none() => o.ty = Some(range),
                (TUPLE_FIELD, _) if c.kind != MUT_RESTRICTION && o.ty.is_none() => o.ty = Some(range),
                (VARIANT | RECORD_FIELD, _) if o.after_eq && o.value.is_none() => o.value = Some(range),
                _ => {}
            }
        }
        self.s.order.truncate(from);
        self.gap(&mut o, at, end, start);
        // The list whose children were made entries is the body the walk reads, or none was.
        assert!(
            list_at == o.body_list_at,
            "a {syntax:?} whose list at token {list_at} is not its body (the body's list is at {})",
            o.body_list_at
        );

        let end_byte = node.end.max(entry_start);
        let rel = |part: Range| relative(part, entry_start);
        let signature_end = match syntax {
            CONST | STATIC => o.before_eq.or(o.before_body),
            _ => o.before_body,
        }
        .or(o.before_semi)
        .unwrap_or(end_byte)
        .max(entry_start);
        let own = &self.out.attrs[attrs.to_usize()];
        let kind = match syntax {
            FN => ItemDetail::Fn { body_holds_items: o.holds_items },
            STRUCT => ItemDetail::Struct,
            ENUM => ItemDetail::Enum,
            UNION => ItemDetail::Union,
            TRAIT if o.before_eq.is_some() => ItemDetail::TraitAlias,
            TRAIT => ItemDetail::Trait,
            IMPL => ItemDetail::Impl,
            CONST => ItemDetail::Const { body_holds_items: o.holds_items },
            STATIC => ItemDetail::Static { body_holds_items: o.holds_items },
            TYPE_ALIAS => ItemDetail::TypeAlias,
            MODULE => ItemDetail::Mod {
                inline: o.body.is_some(),
                path: own.iter().find_map(|attr| match attr.kind {
                    AttrKind::Path { literal } if !attr.inner => Some(literal),
                    _ => None,
                }),
            },
            USE => ItemDetail::Use { tree: o.tree.map(rel) },
            EXTERN_CRATE => ItemDetail::ExternCrate { rename: o.rename.map_or(Rename::Same, |rename| rename.map(rel)) },
            EXTERN_BLOCK => ItemDetail::ExternBlock { abi: o.abi.map(rel) },
            MACRO_RULES => ItemDetail::MacroRules {
                exported: own.iter().any(|attr| !attr.inner && attr.kind == AttrKind::MacroExport),
            },
            MACRO_DEF => ItemDetail::MacroDef,
            MACRO_CALL => ItemDetail::MacroCall { path: o.path.map(rel), input: o.input.map(rel) },
            VARIANT => ItemDetail::Variant { discriminant: o.value.map(rel) },
            RECORD_FIELD | TUPLE_FIELD => ItemDetail::Field { ty: o.ty.map(rel), default: o.value.map(rel) },
            kind if is_error_entry(kind) => ItemDetail::Error,
            syntax => ItemDetail::Unhandled { syntax: u16::from(syntax) },
        };
        let by = match syntax {
            TUPLE_FIELD => By::Position,
            syntax if is_named(syntax) => By::Name(o.name.map(|name| {
                let raw = name.of(self.text).starts_with("r#");
                Range::new(name.start + if raw { 2 } else { 0 }, name.end)
            })),
            _ => By::Head(Range::new(o.head_start.unwrap_or(signature_end).min(signature_end), signature_end)),
        };
        self.scan_to(end);
        let flags = self.take_flags(start, end);
        let parent = match self.s.frames.last() {
            Some(list) if list.row != NONE => Some(ItemIndex(list.row)),
            _ => None,
        };
        self.out.items[row as usize] = Item {
            detail: kind,
            parent,
            end: ItemIndex(narrow(self.out.items.len())),
            range: Range::new(entry_start, end_byte),
            name: o.name.map(rel),
            visibility: o.visibility.unwrap_or(WrittenVis::Private).map(rel),
            signature: Range::new(0, signature_end - entry_start),
            body: o.body.map(rel),
            attrs,
            disambiguator: 0,
            hash: Keyed { tag: tag_of(kind), by }.stash(),
            signature_hash: ContentId([0; 32]),
            flags,
        };
    }

    /// The tokens `from..to` directly in the entry that starts at token `first`.
    fn gap(&self, o: &mut Facts, from: u32, to: u32, first: u32) {
        for at in from as usize..to as usize {
            let start = self.starts[at];
            let before = match at > first as usize {
                true => self.ends[at - 1],
                false => start,
            };
            match self.tokens.kind(at) {
                SEMICOLON => o.before_semi = Some(before),
                EQ => {
                    o.before_eq.get_or_insert(before);
                    o.after_eq = true;
                }
                COLON => o.after_colon = true,
                _ => {}
            }
            o.head_start.get_or_insert(start);
        }
    }

    /// The entry `frame`, given up at token `at`: its row goes, and each node directly in it
    /// is an entry of the list in its place, as the walk finds them there.
    fn abandon_entry(&mut self, frame: Frame, at: u32) {
        assert_eq!(frame.row as usize + 1, self.out.items.len(), "an entry given up holds no entry");
        self.out.items.pop();
        self.out.attrs.truncate(frame.attrs.start as usize);
        self.scan_to(at);
        let list = self.s.frames.last_mut().expect("an entry is in a list");
        list.count -= 1;
        let from = self.children(frame.base, narrow(self.s.nodes.len()));
        for child in (from..self.s.order.len()).rev() {
            let index = self.s.order[child];
            let c = self.s.nodes[index as usize];
            let row = narrow(self.out.items.len());
            self.out.items.push(PLACEHOLDER);
            self.s.frames.last_mut().expect("an entry is in a list").count += 1;
            let attrs = narrow(self.out.attrs.len());
            self.finish(c.kind, c.start, c.end, index - c.kept, index, row, AttrSpan { start: attrs, end: attrs }, NONE, false);
        }
        self.s.order.truncate(from);
    }
}

impl Sink for Rows<'_, '_, '_> {
    fn start(&mut self, at: u32) -> u32 {
        let inner = match self.kind(at as usize) {
            Some(INNER_DOC_COMMENT) => true,
            Some(POUND) => self.kind(at as usize + 1) == Some(BANG),
            _ => false,
        };
        let base = narrow(self.s.nodes.len());
        let mut frame = Frame {
            shape: Shape::Plain,
            holds: false,
            start: at,
            base,
            row: NONE,
            count: 0,
            attrs: AttrSpan { start: 0, end: 0 },
            list: ERROR,
            list_at: NONE,
        };
        if let Some(list) = self.s.frames.last_mut()
            && list.shape == Shape::List
        {
            // An inner attribute before the first item is the owner's; any other node at
            // item level is an entry.
            if inner && is_item_list(list.list) && list.count == 0 {
                frame.shape = Shape::InnerAttr;
            } else {
                list.count += 1;
                frame.shape = Shape::Entry;
                frame.row = narrow(self.out.items.len());
                let attrs = narrow(self.out.attrs.len());
                frame.attrs = AttrSpan { start: attrs, end: attrs };
                self.out.items.push(PLACEHOLDER);
            }
        }
        self.s.frames.push(frame);
        narrow(self.s.frames.len() - 1)
    }

    fn start_list(&mut self, kind: SyntaxKind, at: u32) -> u32 {
        let owner = match self.s.frames.last_mut() {
            None => {
                assert_eq!(kind, SOURCE_FILE, "a source file's parse starts with its root");
                Some(NONE)
            }
            Some(entry) if entry.shape == Shape::Entry && entry.list_at == NONE => {
                entry.list_at = at;
                Some(entry.row)
            }
            Some(_) => None,
        };
        let mark = self.start(at);
        if let Some(row) = owner {
            let frame = self.s.frames.last_mut().expect("the node just opened");
            assert!(frame.shape == Shape::Plain, "a list is directly in an entry, not in a list");
            frame.shape = Shape::List;
            frame.row = row;
            frame.list = kind;
        }
        mark
    }

    #[inline]
    fn token(&mut self, _kind: SyntaxKind, _n_raw_tokens: u8) {}

    fn complete(&mut self, mark: u32, kind: SyntaxKind, at: u32) -> Done {
        let frame = self.pop(mark);
        let mut parent = None;
        if let Some(above) = self.s.frames.last_mut() {
            above.holds |= frame.holds || is_item(kind);
            parent = Some((above.shape, above.start));
        }
        let mut node = Node { kind, holds: frame.holds, start: frame.start, end: at, kept: 0 };
        match frame.shape {
            Shape::Plain => {
                // Every node keeps what is below it: the entry around it may be given up.
                node.kept = narrow(self.s.nodes.len()) - frame.base;
                if let Some((Shape::Entry, first)) = parent
                    && matches!(kind, ATTR | DOC_COMMENT)
                {
                    let base = self.starts[first as usize];
                    let end = self.push_attr(kind, frame.start, at, base);
                    self.s.frames.last_mut().expect("the entry").attrs.end = end;
                }
            }
            Shape::List => {
                assert_eq!(kind, frame.list, "a list is finished as the kind it opened as");
                self.s.nodes.truncate(frame.base as usize);
            }
            Shape::InnerAttr => {
                assert!(matches!(kind, ATTR | DOC_COMMENT), "a {kind:?} where a list's inner attribute starts");
                self.s.nodes.truncate(frame.base as usize);
                // The list's owner: the entry below the list, or the file.
                let owner = self.s.frames.len().checked_sub(2);
                match owner {
                    Some(entry) => {
                        let base = self.starts[self.s.frames[entry].start as usize];
                        let end = self.push_attr(kind, frame.start, at, base);
                        self.s.frames[entry].attrs.end = end;
                    }
                    None => self.out.file_attrs.end = self.push_attr(kind, frame.start, at, 0),
                }
                return Done { start: NONE, end: at };
            }
            Shape::Entry => {
                let hi = narrow(self.s.nodes.len());
                self.finish(kind, frame.start, at, frame.base, hi, frame.row, frame.attrs, frame.list_at, true);
                self.s.nodes.truncate(frame.base as usize);
                return Done { start: NONE, end: at };
            }
        }
        self.s.nodes.push(node);
        Done { start: narrow(self.s.nodes.len() - 1), end: at }
    }

    fn abandon(&mut self, mark: u32, at: u32) {
        let frame = self.pop(mark);
        match frame.shape {
            Shape::Plain => {
                let Some(above) = self.s.frames.last_mut() else { return };
                above.holds |= frame.holds;
                if above.shape != Shape::Entry {
                    return;
                }
                // Its children are the entry's now, each still above its own.
                let base = self.starts.get(above.start as usize).copied();
                let from = self.children(frame.base, narrow(self.s.nodes.len()));
                for child in (from..self.s.order.len()).rev() {
                    let node = self.s.nodes[self.s.order[child] as usize];
                    if matches!(node.kind, ATTR | DOC_COMMENT) {
                        let end = self.push_attr(node.kind, node.start, node.end, base.expect("an attribute's first token started its owner"));
                        self.s.frames.last_mut().expect("the entry").attrs.end = end;
                    }
                }
                self.s.order.truncate(from);
            }
            Shape::Entry => {
                self.abandon_entry(frame, at);
                self.s.nodes.truncate(frame.base as usize);
            }
            Shape::List | Shape::InnerAttr => panic!("the grammar gave up a list or a list's inner attribute at token {at}"),
        }
    }

    fn precede(&mut self, done: u32, _at: u32) -> u32 {
        assert!(done != NONE, "an entry is never wrapped after it is finished");
        let node = self.s.nodes[done as usize];
        let base = done - node.kept;
        if let Some(above) = self.s.frames.last() {
            assert!(above.shape != Shape::List, "a node in a list is never wrapped");
            assert!(
                above.shape != Shape::Entry || (done as usize + 1 == self.s.nodes.len() && !matches!(node.kind, ATTR | DOC_COMMENT)),
                "only the last node of an entry is wrapped, and no attribute"
            );
        }
        // The new node's children are the wrapped one and those finished after it: it opens
        // below the wrapped one's own, which stay where they are.
        let mut holds = false;
        let mut at = narrow(self.s.nodes.len());
        while at > base {
            let child = self.s.nodes[at as usize - 1];
            holds |= child.holds || is_item(child.kind);
            at -= 1 + child.kept;
        }
        self.s.frames.push(Frame {
            shape: Shape::Plain,
            holds,
            start: node.start,
            base,
            row: NONE,
            count: 0,
            attrs: AttrSpan { start: 0, end: 0 },
            list: ERROR,
            list_at: NONE,
        });
        narrow(self.s.frames.len() - 1)
    }

    fn extend_to(&mut self, done: u32, mark: u32) -> u32 {
        let frame = self.pop(mark);
        assert!(frame.shape == Shape::Plain, "only a plain node is taken over by its last child");
        assert_eq!(done as usize + 1, self.s.nodes.len(), "the node extended is the last finished");
        let last = self.s.nodes[done as usize];
        // The nodes the open one held before it are the extended node's first children now, its
        // own after them: all of them are right below it already.
        let before = done - last.kept;
        let holds = last.holds || self.s.nodes[frame.base as usize..before as usize].iter().any(|node| node.holds || is_item(node.kind));
        self.s.nodes[done as usize] = Node { kind: last.kind, holds, start: frame.start, end: last.end, kept: done - frame.base };
        if let Some(above) = self.s.frames.last_mut() {
            above.holds |= holds || is_item(last.kind);
        }
        done
    }

    fn split_float(&mut self, mark: u32, ends_in_dot: bool, at: u32) -> u32 {
        if ends_in_dot {
            return mark;
        }
        // `x.0.1`: the inner field expression ends with the literal, as the walk has it.
        let inner = self.complete(mark, FIELD_EXPR, at + 1);
        self.precede(inner.start, at + 1)
    }

    fn error(&mut self, message: Message, at: u32) {
        let at = self.offset_before(at);
        let range = Range::new(at, at);
        push_error(&mut self.out.errors, &mut self.out.messages, range, ErrorOrigin::Parser, &message);
    }

    fn lazy(&mut self, kind: SyntaxKind, n_input_tokens: u32, at: u32) {
        assert_eq!(kind, BLOCK_EXPR, "an outline leaves only a `fn` body unparsed");
        let (from, len) = (at as usize, n_input_tokens as usize);
        let mut holds = false;
        if may_hold_items(self.tokens, from, len) {
            // The body's words alone, with a pad of their own: a block's parse alone is the
            // subtree a full parse makes there (crates/parser/src/outline.rs).
            let body = &mut self.s.body;
            body.clear();
            body.extend_from_slice(&self.words[from..from + len]);
            body.extend_from_slice(&[Tokens::EOF_WORD; Tokens::PAD]);
            holds = parser::block_into(Tokens::new(body, true), Holds(false)).0;
        }
        if let Some(above) = self.s.frames.last_mut() {
            above.holds |= holds;
        }
        self.s.nodes.push(Node { kind, holds, start: at, end: at + n_input_tokens, kept: 0 });
    }

    fn last_token(&self, end: u32, _input: Tokens<'_>) -> Option<SyntaxKind> {
        end.checked_sub(1).map(|at| self.tokens.kind(at as usize))
    }
}

#[cfg(test)]
mod tests {
    //! The sink driven by hand, as a grammar may drive it: what `parser::Sink` allows is wider
    //! than what today's item grammar does (it gives up an entry only while the entry holds
    //! attributes, a visibility or a restriction, each directly in it), and the sink reads all of
    //! it: nothing the trait allows is refused.

    use super::*;

    /// Each row `drive` leaves of `text`: its detail, its name and how many attributes it has.
    fn driven(text: &str, drive: impl for<'t, 'o> FnOnce(&mut Rows<'t, 'o, 'static>)) -> std::vec::Vec<(ItemDetail, Option<std::string::String>, usize)> {
        let mut buffers = blaze_mem::scratch::lend::<Buffers>();
        let Buffers { out, working } = &mut *buffers;
        let edition = Edition::Edition2024;
        working.columns.fill(&LexedStr::new(edition, text), edition);
        let Columns { words, starts, ends, flags } = &working.columns;
        sink_rows(Seen { text, words, starts, ends, flags }, &mut working.rows, out, |_, mut rows| {
            drive(&mut rows);
            rows
        });
        out.items
            .iter()
            .map(|item| (item.detail, item.name.map(|name| std::string::String::from(item.text(text, name))), item.attrs.to_usize().len()))
            .collect()
    }

    /// `Sink::abandon`: "All its children are attached to its parent instead" (crates/parser
    /// src/parser.rs, `Marker::abandon`). So when the node around a `struct` is given up, and
    /// then the entry around that, the `struct` is a node directly in the list: an entry, read
    /// as any `struct` is, its name the `NAME` in it.
    ///
    /// Prints "test walk::rows::tests::a_node_two_levels_down_is_read_when_it_becomes_an_entry ... ok".
    #[test]
    fn a_node_two_levels_down_is_read_when_it_becomes_an_entry() {
        let rows = driven("struct S;", |rows| {
            let file = rows.start_list(SOURCE_FILE, 0);
            let entry = rows.start(0);
            let around = rows.start(0);
            let node = rows.start(0);
            let name = rows.start(1);
            rows.complete(name, NAME, 2);
            rows.complete(node, STRUCT, 3);
            rows.abandon(around, 3);
            rows.abandon(entry, 3);
            rows.complete(file, SOURCE_FILE, 3);
        });
        assert_eq!(rows, [(ItemDetail::Struct, Some("S".into()), 0)]);
    }

    /// `Sink::extend_to`: the finished node starts where the open one did, so what the open
    /// one held before it is in it (crates/parser src/parser.rs, `CompletedMarker::extend_to`:
    /// "Extends this completed marker *to the left* up to `m`"). The `struct` extended over the
    /// attribute before it holds the attribute and its own name; made an entry when the entry
    /// around it is given up, it has both.
    ///
    /// Prints "test walk::rows::tests::a_node_extended_to_the_left_keeps_both_halves ... ok".
    #[test]
    fn a_node_extended_to_the_left_keeps_both_halves() {
        let rows = driven("#[a] struct S;", |rows| {
            let file = rows.start_list(SOURCE_FILE, 0);
            let entry = rows.start(0);
            let open = rows.start(0);
            let attr = rows.start(0);
            rows.complete(attr, ATTR, 4);
            let node = rows.start(4);
            let name = rows.start(5);
            rows.complete(name, NAME, 6);
            let done = rows.complete(node, STRUCT, 7);
            rows.extend_to(done.start, open);
            rows.abandon(entry, 7);
            rows.complete(file, SOURCE_FILE, 7);
        });
        assert_eq!(rows, [(ItemDetail::Struct, Some("S".into()), 1)]);
    }

    /// `Sink::precede`: the new node opens before the finished one and holds it and every node
    /// finished after it (crates/parser src/parser.rs, `CompletedMarker::precede`). A `struct`
    /// wrapped in a node that is then given up is the entry's own child again, with its name.
    ///
    /// Prints "test walk::rows::tests::a_wrapped_node_keeps_what_is_below_it ... ok".
    #[test]
    fn a_wrapped_node_keeps_what_is_below_it() {
        let rows = driven("struct S;", |rows| {
            let file = rows.start_list(SOURCE_FILE, 0);
            let entry = rows.start(0);
            let around = rows.start(0);
            let node = rows.start(0);
            let name = rows.start(1);
            rows.complete(name, NAME, 2);
            let done = rows.complete(node, STRUCT, 3);
            let wrapper = rows.precede(done.start, 3);
            rows.abandon(wrapper, 3);
            rows.abandon(around, 3);
            rows.abandon(entry, 3);
            rows.complete(file, SOURCE_FILE, 3);
        });
        assert_eq!(rows, [(ItemDetail::Struct, Some("S".into()), 0)]);
    }
}
