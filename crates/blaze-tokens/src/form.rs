//! The record's form: its columns ([`TokensRoot`]), its schema and form number, and the words a
//! token is made of ([`TokenRange`], [`Ctx`], [`CtxNode`], [`Mark`], [`MarkOf`], [`Origin`],
//! [`Lexeme`]), each defined once here for the writer (blaze-expand) and every reader.
//!
//! One rule per fact, used by the builder before it writes and by [`crate::Tokens::open`] once
//! after: [`check_word`], [`needs_lexeme`], [`Ctx::is_in`], [`Origin::from_parts`].

use blaze_format_core::{Part, Record, SchemaHash, Slice};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_surface::{DefSite, Edition, Transparency};
use parser::SyntaxKind;

/// What the columns mean: raised whenever a number in this module changes meaning. A record of
/// another form is refused by its schema ([`TOKENS_SCHEMA`]) and written again (rule 26).
pub const FORM: u64 = 1;

/// The record's schema: its name, [`FORM`] and the root's layout.
pub const TOKENS_SCHEMA: SchemaHash = blaze_format_core::schema_of(&[
    Part::Str("blaze-tokens Tokens"),
    Part::U64(FORM),
    Part::Schema(<TokensRoot as Record>::SCHEMA),
]);

/// The parser's words end in this many `EOF` words ([`parser::Tokens::PAD`]).
pub(crate) const PAD: usize = parser::Tokens::PAD;

/// A token's lexeme column entry when its kind's text is fixed (a punctuation, a delimiter, a
/// keyword): its text is [`SyntaxKind::fixed_text`].
pub(crate) const NO_LEXEME: u32 = u32::MAX;

/// The most tokens a record holds: every index, every end and the pad's words fit a `u32`.
pub(crate) const MAX_TOKENS: u32 = u32::MAX - 8;

/// Where a word holds its syntax extensions: the four bits above [`parser::FRAGMENT`]
/// (parser/src/input.rs asserts `FRAGMENT << 1 == 1 << EXTENSIONS_SHIFT` and a 4-bit field).
/// parser exposes no mask; the report asks it for one.
const EXTENSIONS_SHIFT: u32 = parser::FRAGMENT.trailing_zeros() + 1;
const EXTENSION_BITS: u32 = 0b1111 << EXTENSIONS_SHIFT;

blaze_format_core::record! {
    /// Tokens `start..end` of a token record (or of a view of one: [`crate::Tokens::slice`]),
    /// counted in tokens. A position inside an expansion is one of these (D122 rule 2): an
    /// item's extent, a body's, a nested call's input, a finding's. There is no conversion from
    /// a byte range: a reader that holds bytes holds a file, not an expansion.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct TokenRange: 1 {
        pub start: u32,
        pub end: u32,
    }
}

impl TokenRange {
    /// Tokens `start..end`. Panics when `end < start`, which names no tokens.
    pub const fn new(start: u32, end: u32) -> TokenRange {
        assert!(start <= end, "a token range that ends before it starts");
        TokenRange { start, end }
    }

    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Whether `inner` lies inside this range.
    pub const fn contains(self, inner: TokenRange) -> bool {
        self.start <= inner.start && inner.end <= self.end
    }
}

/// A token's hygiene context: the root of an edition (a token no expansion made), or a node of
/// its record's context table. The same numbering blaze-expand's contexts column holds today.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ctx(u32);

impl Ctx {
    /// The values below this are the roots, one per [`Edition`] (its byte).
    pub(crate) const ROOTS: u32 = 4;

    /// The root context of `edition`.
    pub fn root(edition: Edition) -> Ctx {
        Ctx(u32::from(edition.byte()))
    }

    /// The context of node `index` of a table.
    pub(crate) const fn of_node(index: u32) -> Ctx {
        Ctx(index + Ctx::ROOTS)
    }

    /// The edition of a root context; `None` for a node.
    pub fn root_edition(self) -> Option<Edition> {
        match self.0 < Ctx::ROOTS {
            true => Edition::from_byte(u8::try_from(self.0).ok()?),
            false => None,
        }
    }

    /// The index of a node in its record's table; `None` for a root.
    pub const fn node(self) -> Option<u32> {
        self.0.checked_sub(Ctx::ROOTS)
    }

    /// The context as the record holds it.
    pub const fn raw(self) -> u32 {
        self.0
    }

    pub(crate) const fn from_raw(raw: u32) -> Ctx {
        Ctx(raw)
    }

    /// Whether `raw` names a root or one of the first `nodes` nodes of a table.
    pub(crate) fn is_in(raw: u32, nodes: usize) -> bool {
        match raw.checked_sub(Ctx::ROOTS) {
            // Below `ROOTS`: each of the four values is an edition's byte.
            None => true,
            Some(node) => at(node) < nodes,
        }
    }
}

/// Which expansion a mark is of.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MarkOf {
    /// The expansion the record is the output of, wherever it is placed.
    This,
    /// An earlier expansion, by its record's key.
    Outer(ContentId),
}

/// One expansion's mark on a context.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mark {
    pub of: MarkOf,
    pub transparency: Transparency,
    /// The edition of the definition: what the token is read in.
    pub edition: Edition,
    pub site: DefSite,
}

/// A context of a record's table: a mark over a parent context, which is a root or an earlier
/// node.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CtxNode {
    pub parent: Ctx,
    pub mark: Mark,
}

/// Where a token of an expansion came from, by token index into one of the call's or the
/// definition's streams, never a byte position (rule 71): blaze-expand's `TokenOrigin`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Origin {
    /// Token `n` of the call's input.
    Input(u32),
    /// Token `n` of the attribute's arguments.
    AttrArgs(u32),
    /// Token `n` of the definition's body.
    Def(u32),
    /// Token `n` of a `macro m(params)` definition's parameters.
    Params(u32),
    /// Made by the expansion, with this transparency.
    Made(Transparency),
}

impl Origin {
    /// Its tag and value, as the two origin columns hold them.
    pub(crate) fn parts(self) -> (u8, u32) {
        match self {
            Origin::Input(at) => (0, at),
            Origin::AttrArgs(at) => (1, at),
            Origin::Def(at) => (2, at),
            Origin::Params(at) => (3, at),
            Origin::Made(transparency) => (4, u32::from(transparency.byte())),
        }
    }

    /// What [`Origin::parts`] wrote; `None` for a pair it does not write.
    pub(crate) fn from_parts(tag: u8, value: u32) -> Option<Origin> {
        Some(match tag {
            0 => Origin::Input(value),
            1 => Origin::AttrArgs(value),
            2 => Origin::Def(value),
            3 => Origin::Params(value),
            4 => Origin::Made(Transparency::from_byte(u8::try_from(value).ok()?)?),
            _ => return None,
        })
    }
}

/// A lexeme of a record's table, by its number: first-seen order, so two builders of the same
/// tokens number them alike. Two tokens of one record with one lexeme have equal numbers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Lexeme(pub(crate) u32);

impl Lexeme {
    pub const fn index(self) -> u32 {
        self.0
    }
}

blaze_format_core::record! {
    /// A node of the context table: its mark's keys (zero where [`NodeRow::outer`] or
    /// [`NodeRow::has_module`] is false), its parent's raw [`Ctx`], and the mark's words.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct NodeRow: 1 {
        pub(crate) of: ContentId,
        pub(crate) krate: ContentId,
        pub(crate) module: ContentId,
        pub(crate) parent: u32,
        pub(crate) transparency: Transparency,
        pub(crate) edition: Edition,
        pub(crate) outer: bool,
        pub(crate) has_module: bool,
    }
}

const ZERO: ContentId = ContentId([0; 32]);

impl NodeRow {
    pub(crate) fn of(node: CtxNode) -> NodeRow {
        let Mark { of, transparency, edition, site } = node.mark;
        let (outer, of) = match of {
            MarkOf::This => (false, ZERO),
            MarkOf::Outer(key) => (true, key),
        };
        NodeRow {
            of,
            krate: site.krate.0,
            module: site.module.map_or(ZERO, |module| module.0),
            parent: node.parent.raw(),
            transparency,
            edition,
            outer,
            has_module: site.module.is_some(),
        }
    }

    pub(crate) fn node(self) -> CtxNode {
        CtxNode {
            parent: Ctx::from_raw(self.parent),
            mark: Mark {
                of: match self.outer {
                    true => MarkOf::Outer(self.of),
                    false => MarkOf::This,
                },
                transparency: self.transparency,
                edition: self.edition,
                site: DefSite {
                    krate: CrateItemKey(self.krate),
                    module: self.has_module.then_some(ItemKey(self.module)),
                },
            },
        }
    }

    /// Why row `index` is not one [`NodeRow::of`] writes into a table: its parent not a root or
    /// an earlier node, or a key where its flag says none.
    pub(crate) fn refused(self, index: usize) -> Option<&'static str> {
        if !Ctx::is_in(self.parent, index) {
            return Some("a context whose parent is not a root or an earlier context");
        }
        if !self.outer && self.of != ZERO {
            return Some("a mark of this expansion that names an outer one");
        }
        if !self.has_module && self.module != ZERO {
            return Some("a definition site with a module its flag says it has not");
        }
        None
    }
}

blaze_format_core::record! {
    /// The root: one column per fact, each a slice of the buffer.
    ///
    /// - `words`: per token its parser word, then [`PAD`] words of `EOF`: n + 4 entries.
    /// - `lexeme_of`: per token its lexeme's number, [`NO_LEXEME`] for a kind of fixed text.
    /// - `ctxs`: per token its raw [`Ctx`].
    /// - `origin_values`, `origin_tags`: per token its [`Origin`] ([`Origin::parts`]).
    /// - `joined`: bit `i % 64` of word `i / 64` set when token `i` is joined to the next;
    ///   `ceil(n / 64)` words, bits past the last token clear.
    /// - `lexeme_ends`: per lexeme the end of its bytes in `lexeme_bytes` (lexeme 0 starts
    ///   at 0), non-decreasing, the last one `lexeme_bytes`' length.
    /// - `lexeme_bytes`: every distinct lexeme's UTF-8, once, in first-seen order.
    /// - `nodes`: the context table, each parent before its children.
    /// - `groups`: the invisible groups (a `macro_rules!` fragment passed on), as token ranges
    ///   sorted by start, an outer group before the groups it holds.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct TokensRoot: 1 {
        pub(crate) words: Slice<u32>,
        pub(crate) lexeme_of: Slice<u32>,
        pub(crate) ctxs: Slice<u32>,
        pub(crate) origin_values: Slice<u32>,
        pub(crate) origin_tags: Slice<u8>,
        pub(crate) joined: Slice<u64>,
        pub(crate) lexeme_ends: Slice<u32>,
        pub(crate) lexeme_bytes: Slice<u8>,
        pub(crate) nodes: Slice<NodeRow>,
        pub(crate) groups: Slice<TokenRange>,
    }
}

/// The kind of `word` when it is a word [`parser::Tokens::new`] takes and no `EOF`; else what is
/// wrong with it. Its [`parser::JOINT`], [`parser::FRAGMENT`] and syntax-extension bits are
/// kept as the writer made them (blaze-expand's `words_of`): they are the parser's.
pub(crate) fn check_word(word: u32) -> Result<SyntaxKind, &'static str> {
    let fields = word & !(parser::JOINT | parser::FRAGMENT | EXTENSION_BITS);
    match parser::token_word(fields) {
        Err(parser::WordRefused::Kind) => return Err("a word whose kind is no SyntaxKind"),
        Err(parser::WordRefused::Edition) => return Err("a word whose edition is no Edition"),
        Err(parser::WordRefused::Extensions) => {
            return Err("a word whose syntax extensions are outside their bits");
        }
        Ok(known) if known != fields => return Err("a word with bits set that no field uses"),
        Ok(_) => {}
    }
    let extensions = u8::try_from((word & EXTENSION_BITS) >> EXTENSIONS_SHIFT)
        .expect("four bits fit a byte");
    if parser::Extensions::from_bits(extensions).is_none() {
        return Err("a word marked with syntax extensions that are none");
    }
    match parser::word_kind(word) {
        SyntaxKind::EOF => Err("an EOF word among the tokens"),
        kind => Ok(kind),
    }
}

/// Whether a token of `kind` has a lexeme: its text is not fixed by its kind (an identifier, a
/// lifetime, a literal, an error token). A punctuation, a delimiter and a keyword have none.
pub(crate) fn needs_lexeme(kind: SyntaxKind) -> bool {
    kind.fixed_text().is_none()
}

/// The bytes of lexeme `k` of a table cut by `ends`.
pub(crate) fn lexeme_span(ends: &[u32], k: usize) -> core::ops::Range<usize> {
    let start = match k {
        0 => 0,
        k => at(ends[k - 1]),
    };
    start..at(ends[k])
}

/// `value` as an index.
pub(crate) fn at(value: u32) -> usize {
    usize::try_from(value).expect("a u32 fits a usize on every target blaze runs on")
}

/// `value` as a record's count.
pub(crate) fn wide(value: usize) -> u64 {
    u64::try_from(value).expect("a usize fits a u64 on every target blaze runs on")
}
