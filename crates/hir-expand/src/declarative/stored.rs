//! A declarative macro's definition as entries (AGENTS.md rule 65): its tokens found by what
//! the definition is, so a crate that calls `vec!` makes the expander without a parse of the
//! file `vec!` is written in (docs/v2/plan/design/restore.md, "Items read in place", row D).
//!
//! Neither of the two things read is keyed by the definition's crate:
//!
//! - the definition's row in its file's index ([`index`]), found by the file's content: the
//!   position-free hash of the definition's text ([`def_hash`]), with no parse.
//! - the *tokens* (`macro def tokens`), under that hash, the ast id, the file's edition and
//!   extensions and the defining crate's edition: the definition's transparency and its token
//!   trees. An edit elsewhere in the file moves the file's index and finds the tokens again.
//!
//! The tokens name no file, no crate and no syntax context: every span of a kept definition
//! is of the root context of the file it is written in and relative to the definition's own
//! ast id, so a span is two numbers and the reader gives the rest. Nor do they hold where a
//! token is in the definition's text: the key is of the definition's tokens, never its
//! whitespace and plain comments ([`index`]), so a span is kept as offsets in the text with
//! those squeezed out ([`Squeeze`]), and read back to the definition's text as its file has
//! it now, which the reader lexes from where the file's index places the definition. Two
//! definitions of equal tokens, reindented or commented apart, are one key and equal bytes; a
//! definition moved or reindented is found by its key and its spans point at its tokens where
//! they are now. A definition that is not
//! so (one a macro wrote, one whose spans have a wider anchor) is not kept and is made from
//! its source, as before. A definition whose text names `cfg` (a `cfg_attr` deciding its
//! transparency) is keyed by its crate's cfg too ([`cfg_part`]). Its content hash
//! ([`def_hash`]) is read from its expansion's index where a macro wrote it and the expansion
//! has a key ([`expansion`]).
//!
//! The invariant: equal keys are equal bytes; the tokens are a function of the definition's
//! tokens and its ast id alone, never of its whitespace and plain comments.

use std::{alloc::Global, sync::Arc};

use base_db::{Crate, Db, EditionedFileId};
use blaze_ids::{ContentId, Hasher};
use intern::Symbol;
use span::{Edition, ErasedFileAstId, Span, SpanAnchor, SyntaxContext};
use syntax::{
    TextRange, TextSize, ast,
    view,
};

use super::DeclarativeMacroExpander;
use crate::{AstId, HirFileId, hygiene::Transparency, tt};

type R<T = ()> = Result<T, String>;

pub mod expansion;
pub mod index;

/// The layout of both entries. Part of their keys, and the first word of the tokens. 2: the
/// defining crate's cfg in the tokens' key where the definition's text names `cfg`, so a
/// definition whose transparency a `cfg_attr` decides is kept too. 3: a span by its offsets in
/// the definition's text with whitespace and plain comments squeezed out ([`Squeeze`]), where
/// 2 held its offsets in the text, which the key (of tokens, index layout 4) does not cover:
/// two definitions of equal tokens spaced apart were two values under one key.
pub const LAYOUT: u32 = 3;

/// The family of a definition's tokens.
pub const TOKENS: &str = "macro def tokens";

const SET_DOMAIN: &str = "blaze v2 file entry set";
const TOKENS_DOMAIN: &str = "blaze v2 macro def tokens";

/// Where definitions are kept: hir-def's entry store (`nameres::frozen::entries`), the one
/// place entries are put and got, reached from here through this.
pub trait Kept: Send + Sync + 'static {
    /// Calls `with` once with the entry of `family` under `key` and says `true`; `false`,
    /// with no call, when none is kept. An entry kept that does not read (`with` said why)
    /// unwinds with the implementor's typed error naming `krate`, the family and the key.
    // dyn: one call per entry read
    fn get(
        &self,
        db: &Db,
        krate: Crate,
        family: &'static str,
        set: &ContentId,
        key: ContentId,
        with: &mut dyn FnMut(&[u8]) -> R,
    ) -> bool;

    /// Keeps `bytes` as the entry of `family` under `key`, packed with the entries of `set`.
    fn put(
        &self,
        db: &Db,
        krate: Crate,
        family: &'static str,
        set: &ContentId,
        key: ContentId,
        bytes: &[u8],
    );
}

/// Where this database's macro definitions are kept, set by `entries::set` with the entry
/// store. One value a database.
#[blaze_query::input(singleton, debug)]
pub struct KeptDefs {
    #[returns(ref)]
    pub held: Holder,
}

/// A [`Kept`] as an input's value: equal only to itself.
#[derive(Clone)]
// dyn: the one holder
pub struct Holder(pub Arc<dyn Kept>);

impl PartialEq for Holder {
    fn eq(&self, other: &Holder) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Holder {}

impl std::fmt::Debug for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Kept")
    }
}

/// Has `db` keep and find its macro definitions in `kept`.
// dyn: the one holder
pub fn set(db: &mut Db, kept: Arc<dyn Kept>) {
    use blaze_query::db::{Durability, Setter};
    match KeptDefs::try_get(&*db) {
        Some(it) => {
            it.set_held(db).with_durability(Durability::HIGH).to(Holder(kept));
        }
        None => {
            _ = KeptDefs::new(db, Durability::HIGH, Holder(kept));
        }
    }
}

/// Where the macro definitions of `db` are kept, if anything said.
// dyn: the one holder
pub fn of(db: &Db) -> Option<&dyn Kept> {
    KeptDefs::try_get(db).map(|it| &*it.held(db).0)
}

/// The edition and syntax extensions `file` is read under, as one byte.
pub fn read_as(db: &Db, file: EditionedFileId) -> u8 {
    file.edition(db) as u8 | file.extensions(db).bits() << 4
}

/// The set the entries of `file`'s items are packed in: its content and how it is read.
pub fn file_set(db: &Db, file: EditionedFileId) -> ContentId {
    let mut hasher = Hasher::new(SET_DOMAIN);
    hasher.id(&db.file_content(file.file_id(db))).update(&[read_as(db, file)]);
    hasher.finish()
}

/// The set the entries of an item written in `file` are packed in, and how its text is read:
/// a file's [`file_set`] and [`read_as`], or an expansion's key ([`expansion::key`]) and
/// [`expansion::EXPANDED`]. The one home of both for every per-item entry. `None` for an
/// expansion with no key.
pub fn set_of(db: &Db, file: HirFileId) -> Option<(ContentId, u8)> {
    match file {
        HirFileId::FileId(file) => Some((file_set(db, file), read_as(db, file))),
        HirFileId::MacroFile(call) => Some((expansion::key(db, call)?, expansion::EXPANDED)),
    }
}

/// The key of the tokens of the definition whose tree hashes to `hash`, with ast id `ast`,
/// read as `read_as` and compiled for a defining crate of `def_edition`, with that crate's cfg
/// `cfg` where the definition's text names `cfg` ([`cfg_part`]).
pub fn tokens_key(
    hash: u128,
    ast: ErasedFileAstId,
    read_as: u8,
    def_edition: Edition,
    cfg: Option<&ContentId>,
) -> ContentId {
    let mut hasher = Hasher::new(TOKENS_DOMAIN);
    hasher
        .update(&LAYOUT.to_le_bytes())
        .update(&hash.to_le_bytes())
        .update(&ast.into_raw().to_le_bytes())
        .update(&[read_as, def_edition as u8]);
    match cfg {
        Some(cfg) => hasher.update(&[1]).id(cfg),
        None => hasher.update(&[0]),
    };
    hasher.finish()
}

/// The cfg of `def_crate` where the text of the definition `ast` of `file` names `cfg` (its
/// row's `attrs_cfg` or `header_cfg`): what a `cfg_attr` on it reads, so its tokens' key takes
/// it. `None` where the text names no `cfg`, whose tokens are the same under every cfg.
pub(super) fn cfg_part(
    db: &Db,
    file: EditionedFileId,
    ast: ErasedFileAstId,
    def_crate: Crate,
) -> Option<ContentId> {
    let source = index::of(db, file, def_crate).get(ast)?;
    (source.attrs_cfg || source.header_cfg)
        .then(|| base_db::cfg_options_content(def_crate.cfg_options(db)))
}

/// The content hash of the macro definition `id` of `def_crate`: what a key that depends on
/// which definition a call expanded covers (a body's `MACRO_DEF` fact). The `header` of the
/// definition's [`index::ItemSource`]: from its file's index, or its expansion's where a macro
/// wrote it and the expansion has a key ([`expansion::key`]), with no parse or expansion where
/// the index is kept; of a definition an expansion with no key wrote, or in a database that
/// keeps no entries, from its tree, the same value.
pub fn def_hash(db: &Db, id: AstId<ast::Macro>, def_crate: Crate) -> u128 {
    if of(db).is_some()
        && let Some(source) = index::source(db, id.file_id, id.value.erase(), def_crate)
    {
        return source.header;
    }
    index::item_source(match id.to_view(db) {
        view::Macro::MacroRules(it) => view::Item::MacroRules(it),
        view::Macro::MacroDef(it) => view::Item::MacroDef(it),
    })
    .header
}

/// The kept tokens of the definition `ast` of `file`, whose tree hashes to `hash`
/// ([`def_hash`]), if they are kept: no syntax is made.
pub(super) fn find(
    db: &Db,
    kept: &dyn Kept,
    file: EditionedFileId,
    ast: ErasedFileAstId,
    def_crate: Crate,
    hash: u128,
) -> Option<Def> {
    // A definition with no row has no text to read its spans back to: none is kept of it
    // ([`Writer::new`]).
    let place = Place::of(db, file, ast, def_crate)?;
    let set = file_set(db, file);
    let def_edition = def_crate.data(db).edition;
    let cfg = cfg_part(db, file, ast, def_crate);
    let key = tokens_key(hash, ast, read_as(db, file), def_edition, cfg.as_ref());
    let mut def = None;
    kept.get(db, def_crate, TOKENS, &set, key, &mut |bytes| {
        def = Some(decode(bytes, &place)?);
        Ok(())
    });
    def
}

/// Keeps the tokens of the definition `ast` of `file`, whose tree hashes to `hash`, as
/// `written` wrote them, when every span was the definition's own.
pub(super) fn keep(
    db: &Db,
    kept: &dyn Kept,
    file: EditionedFileId,
    ast: ErasedFileAstId,
    def_crate: Crate,
    hash: u128,
    written: Writer,
) {
    if let Some(bytes) = written.finish() {
        let set = file_set(db, file);
        let cfg = cfg_part(db, file, ast, def_crate);
        let edition = def_crate.data(db).edition;
        let key = tokens_key(hash, ast, read_as(db, file), edition, cfg.as_ref());
        kept.put(db, def_crate, TOKENS, &set, key, &bytes);
    }
}

/// What every span of a kept definition shares: the file, the definition as the anchor and
/// the file's root context; and where its tokens are in its text ([`Squeeze`]).
#[derive(Clone)]
pub(super) struct Place {
    anchor: SpanAnchor,
    ctx: SyntaxContext,
    squeeze: Squeeze,
}

impl Place {
    /// The place of the definition `ast` of `file`: its text is where its file's index places
    /// it (its node's range, where its anchor begins, so a span's offsets are in that text),
    /// read from the file's text with no parse. `None` for a definition with no row (one in a
    /// body, whose spans have another anchor and are not kept).
    pub(super) fn of(
        db: &Db,
        file: EditionedFileId,
        ast: ErasedFileAstId,
        def_crate: Crate,
    ) -> Option<Place> {
        let range = index::of(db, file, def_crate).range(ast)?;
        let text: &str = db.file_text(file.file_id(db)).text(db);
        let text = text.get(std::ops::Range::<usize>::from(range))?;
        Some(Place {
            anchor: SpanAnchor { file_id: file.span_file_id(db), ast_id: ast },
            ctx: SyntaxContext::root(file.edition(db)),
            squeeze: Squeeze::of(text),
        })
    }
}

/// Where the tokens of a definition's text are: each token's start in the text, its start in
/// the text with whitespace and plain comments squeezed out, and its length, in order. The
/// tokens are those its row hashes ([`index::token_ranges`]), so the squeezed text is a
/// function of the row's hash: equal keys squeeze to equal offsets.
///
/// An offset in a token maps to the same offset in it, squeezed, and back: a span within a
/// token or from one token to another is kept exactly, and read back to where those tokens
/// are now. A start is at a byte of a token, an end just past one; an offset in whitespace or a
/// comment maps to none, and the definition is not kept ([`Writer::span`]).
#[derive(Clone)]
pub(super) struct Squeeze {
    tokens: Vec<[u32; 3], Global>,
}

impl Squeeze {
    /// The squeeze of `text`. One lex of the definition, one entry a token, on the global
    /// heap: the place lives as long as the writer or the read that holds it.
    pub(super) fn of(text: &str) -> Squeeze {
        let mut tokens = Vec::new_in(Global);
        let mut squeezed = 0u32;
        index::token_ranges(text, |start, len| {
            tokens.push([start, squeezed, len]);
            squeezed += len;
        });
        Squeeze { tokens }
    }

    /// `at`, an offset on side `from` (0 the text, 1 the squeezed text), on the other side: as
    /// a start (in a token) or an end (just past a byte of one). `None` elsewhere.
    fn map(&self, at: u32, end: bool, from: usize) -> Option<u32> {
        let to = 1 - from;
        let i = self.tokens.partition_point(|t| {
            let past = t[from] + t[2];
            if end { past < at } else { past <= at }
        });
        let t = self.tokens.get(i)?;
        let inside = if end { t[from] < at } else { t[from] <= at };
        inside.then(|| t[to] + (at - t[from]))
    }

    /// The range `range` of the text, squeezed: `None` where an end is in no token.
    fn squeeze(&self, range: TextRange) -> Option<(u32, u32)> {
        let start = self.map(range.start().into(), false, 0)?;
        let end = if range.is_empty() { start } else { self.map(range.end().into(), true, 0)? };
        Some((start, end))
    }

    /// The squeezed range `start..end` in the text: `None` where an end is past every token.
    fn unsqueeze(&self, start: u32, end: u32) -> Option<TextRange> {
        let real = self.map(start, false, 1)?;
        let real_end = if start == end { real } else { self.map(end, true, 1)? };
        Some(TextRange::new(TextSize::new(real), TextSize::new(real_end)))
    }
}

/// How a definition is written.
const RULES_WITHOUT_TREE: u8 = 0;
const RULES: u8 = 1;
const MACRO_WITHOUT_BODY: u8 = 2;
const MACRO_BODY: u8 = 3;
const MACRO_ARGS_AND_BODY: u8 = 4;

/// A definition read from its tokens entry.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Def {
    transparency: Transparency,
    shape: Shape,
}

#[derive(Debug, PartialEq, Eq)]
enum Shape {
    RulesWithoutTree,
    Rules(tt::TopSubtree),
    MacroWithoutBody,
    Macro { args: Option<tt::TopSubtree>, body: tt::TopSubtree },
}

impl Def {
    /// The expander of the definition, for a defining crate of `def_edition`: what
    /// `decl_macro_expander` makes of the definition's syntax. Every context of a kept
    /// definition is its file's root, whose edition for the rules is the defining crate's.
    pub(super) fn compile(self, def_edition: Edition) -> DeclarativeMacroExpander {
        let ctx_edition = move |_: SyntaxContext| def_edition;
        let no_tree = || {
            mbe::DeclarativeMacro::from_err(mbe::ParseError::Expected(
                "expected a token tree".into(),
            ))
        };
        let mac = match &self.shape {
            Shape::RulesWithoutTree | Shape::MacroWithoutBody => no_tree(),
            Shape::Rules(tt) => mbe::DeclarativeMacro::parse_macro_rules(tt, ctx_edition),
            Shape::Macro { args, body } => {
                mbe::DeclarativeMacro::parse_macro2(args.as_ref(), body, ctx_edition)
            }
        };
        DeclarativeMacroExpander { mac, transparency: self.transparency, edition: def_edition }
    }
}

/// Writes a definition's tokens entry as `decl_macro_expander` makes its trees.
pub(super) struct Writer {
    out: Vec<u8>,
    place: Place,
    /// Every span so far was the definition's own ([`Place`]).
    own: bool,
}

impl Writer {
    pub(super) fn new(place: Place) -> Writer {
        // alloc: an entry's bytes, once per definition whose tokens are not kept yet
        let mut out = Vec::new();
        out.extend_from_slice(&LAYOUT.to_le_bytes());
        // The transparency and the shape, said when they are known.
        out.extend_from_slice(&[0, 0]);
        Writer { out, place, own: true }
    }

    /// The definition is not kept: something it is made of is not in its key (its
    /// transparency read the crate's cfg).
    pub(super) fn disown(&mut self) {
        self.own = false;
    }

    pub(super) fn transparency(&mut self, it: Transparency) {
        self.out[4] = match it {
            Transparency::Transparent => 0,
            Transparency::SemiOpaque => 1,
            Transparency::Opaque => 2,
        };
    }

    pub(super) fn rules<A: std::alloc::Allocator>(&mut self, tt: Option<&tt::TopSubtree<A>>) {
        match tt {
            None => self.out[5] = RULES_WITHOUT_TREE,
            Some(tt) => {
                self.out[5] = RULES;
                self.tree(tt);
            }
        }
    }

    pub(super) fn macro_without_body(&mut self) {
        self.out[5] = MACRO_WITHOUT_BODY;
    }

    pub(super) fn macro2<A: std::alloc::Allocator, B: std::alloc::Allocator>(
        &mut self,
        args: Option<&tt::TopSubtree<A>>,
        body: &tt::TopSubtree<B>,
    ) {
        match args {
            None => self.out[5] = MACRO_BODY,
            Some(args) => {
                self.out[5] = MACRO_ARGS_AND_BODY;
                self.tree(args);
            }
        }
        self.tree(body);
    }

    /// The entry's bytes; `None` when a span was not the definition's own.
    pub(super) fn finish(self) -> Option<Vec<u8>> {
        self.own.then_some(self.out)
    }

    fn u32(&mut self, it: u32) {
        self.out.extend_from_slice(&it.to_le_bytes());
    }

    fn str(&mut self, it: &str) {
        match u32::try_from(it.len()) {
            Ok(len) => {
                self.u32(len);
                self.out.extend_from_slice(it.as_bytes());
            }
            // A token of 4 GiB: not kept.
            Err(_) => self.own = false,
        }
    }

    /// A span as its offsets in the definition's text squeezed ([`Squeeze`]): the key holds
    /// the tokens and not the spaces between them, so neither does the value. A span of
    /// another anchor or context, or one that ends outside every token, is not the
    /// definition's own, and the definition is not kept.
    fn span(&mut self, it: Span) {
        if it.anchor != self.place.anchor || it.ctx != self.place.ctx {
            self.own = false;
        }
        let (start, end) = match self.place.squeeze.squeeze(it.range) {
            Some(it) => it,
            None => {
                self.own = false;
                (0, 0)
            }
        };
        self.u32(start);
        self.u32(end - start);
    }

    fn tree<A: std::alloc::Allocator>(&mut self, tt: &tt::TopSubtree<A>) {
        let tokens = tt.as_token_trees();
        match u32::try_from(tokens.len()) {
            Ok(len) => self.u32(len),
            Err(_) => self.own = false,
        }
        for token in tokens.iter_flat_tokens() {
            if !self.own {
                return;
            }
            match token {
                tt::TokenTree::Subtree(tt::Subtree {
                    delimiter: tt::Delimiter { open, close, kind },
                    len,
                }) => {
                    self.out.extend_from_slice(&[0, kind as u8]);
                    self.u32(len);
                    self.span(open);
                    self.span(close);
                }
                tt::TokenTree::Leaf(tt::Leaf::Literal(tt::Literal {
                    text_and_suffix,
                    span,
                    kind,
                    suffix_len,
                })) => {
                    let (kind, hashes) = kind.code();
                    self.out.extend_from_slice(&[1, kind, hashes]);
                    self.u32(suffix_len);
                    self.str(text_and_suffix.as_str());
                    self.span(span);
                }
                tt::TokenTree::Leaf(tt::Leaf::Punct(tt::Punct { char, spacing, span })) => {
                    self.out.extend_from_slice(&[2, spacing as u8]);
                    self.u32(char as u32);
                    self.span(span);
                }
                tt::TokenTree::Leaf(tt::Leaf::Ident(tt::Ident { sym, span, is_raw })) => {
                    self.out.extend_from_slice(&[3, is_raw as u8]);
                    self.str(sym.as_str());
                    self.span(span);
                }
            }
        }
    }
}

/// Reads a tokens entry.
struct In<'a> {
    bytes: &'a [u8],
    place: &'a Place,
}

impl<'a> In<'a> {
    fn take(&mut self, len: usize) -> R<&'a [u8]> {
        if self.bytes.len() < len {
            // alloc: error path
            return Err(format!("{len} bytes asked of the {} left", self.bytes.len()));
        }
        let (taken, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(taken)
    }

    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> R<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| "four bytes")?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn symbol(&mut self) -> R<Symbol> {
        let len = self.u32()? as usize;
        let text = std::str::from_utf8(self.take(len)?).map_err(|_| "a token that is not UTF-8")?;
        Ok(Symbol::intern(text))
    }

    /// A span kept squeezed ([`Writer::span`]), read back to where its tokens are in the
    /// definition's text as its file has it now.
    fn span(&mut self) -> R<Span> {
        let start = self.u32()?;
        let len = self.u32()?;
        let end = start.checked_add(len).ok_or("a span past 4 GiB")?;
        let range = self.place.squeeze.unsqueeze(start, end).ok_or_else(|| {
            // alloc: error path
            format!("a span at {start}..{end} of the squeezed text, past the definition's tokens")
        })?;
        Ok(Span { range, anchor: self.place.anchor, ctx: self.place.ctx })
    }

    fn tree(&mut self) -> R<tt::TopSubtree> {
        let len = self.u32()? as usize;
        // Each token is nine bytes or more: a length the bytes cannot hold is refused before
        // anything is reserved for it.
        if len > self.bytes.len() / 9 {
            // alloc: error path
            return Err(format!("{len} tokens in {} bytes", self.bytes.len()));
        }
        // alloc: the decoded tree's tokens, once per definition read
        let mut tokens = Vec::with_capacity(len);
        for _ in 0..len {
            let token = match self.u8()? {
                0 => {
                    let kind = match self.u8()? {
                        0 => tt::DelimiterKind::Parenthesis,
                        1 => tt::DelimiterKind::Brace,
                        2 => tt::DelimiterKind::Bracket,
                        3 => tt::DelimiterKind::Invisible,
                        // alloc: error path
                        other => return Err(format!("delimiter kind {other}")),
                    };
                    let len = self.u32()?;
                    let (open, close) = (self.span()?, self.span()?);
                    tt::TokenTree::Subtree(tt::Subtree {
                        delimiter: tt::Delimiter { open, close, kind },
                        len,
                    })
                }
                1 => {
                    let (kind, hashes) = (self.u8()?, self.u8()?);
                    let kind = tt::LitKind::from_code(kind, hashes)
                        // alloc: error path
                        .ok_or_else(|| format!("literal kind {kind}"))?;
                    let suffix_len = self.u32()?;
                    let text_and_suffix = self.symbol()?;
                    if suffix_len as usize > text_and_suffix.as_str().len() {
                        // alloc: error path
                        return Err("a literal's suffix longer than the literal".to_owned());
                    }
                    let span = self.span()?;
                    tt::Leaf::Literal(tt::Literal { text_and_suffix, span, kind, suffix_len }).into()
                }
                2 => {
                    let spacing = match self.u8()? {
                        0 => tt::Spacing::Alone,
                        1 => tt::Spacing::Joint,
                        2 => tt::Spacing::JointHidden,
                        // alloc: error path
                        other => return Err(format!("spacing {other}")),
                    };
                    let char = char::from_u32(self.u32()?).ok_or("a punct that is no char")?;
                    let span = self.span()?;
                    tt::Leaf::Punct(tt::Punct { char, spacing, span }).into()
                }
                3 => {
                    let is_raw = match self.u8()? {
                        0 => tt::IdentIsRaw::No,
                        1 => tt::IdentIsRaw::Yes,
                        // alloc: error path
                        other => return Err(format!("raw flag {other}")),
                    };
                    let sym = self.symbol()?;
                    let span = self.span()?;
                    tt::Leaf::Ident(tt::Ident { sym, span, is_raw }).into()
                }
                // alloc: error path
                other => return Err(format!("token tag {other}")),
            };
            tokens.push(token);
        }
        check_shape(&tokens)?;
        Ok(tt::TopSubtree::from_serialized(tokens))
    }
}

/// Refuses tokens that are no tree: the first is not the top subtree over all the rest, or a
/// subtree is longer than what is after it inside its parent.
fn check_shape(tokens: &[tt::TokenTree]) -> R {
    let Some(tt::TokenTree::Subtree(top)) = tokens.first() else {
        // alloc: error path
        return Err("a tree with no top subtree".to_owned());
    };
    if top.len as usize != tokens.len() - 1 {
        // alloc: error path
        return Err("a top subtree that is not over every token".to_owned());
    }
    // The end of each subtree that is open, innermost last.
    // alloc: one end a nesting level, once per definition read
    let mut ends: Vec<usize> = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        while ends.last() == Some(&at) {
            ends.pop();
        }
        if let tt::TokenTree::Subtree(it) = token {
            let end = at + 1 + it.len as usize;
            if end > ends.last().copied().unwrap_or(tokens.len()) {
                // alloc: error path
                return Err("a subtree longer than the subtree it is in".to_owned());
            }
            ends.push(end);
        }
    }
    Ok(())
}

/// Reads the tokens entry `bytes` of the definition at `place`. A layout of another blaze is
/// refused by name.
pub(super) fn decode(bytes: &[u8], place: &Place) -> R<Def> {
    let mut r = In { bytes, place };
    let layout = r.u32()?;
    if layout != LAYOUT {
        // alloc: error path
        return Err(format!("macro def tokens of layout {layout}, and this blaze reads {LAYOUT}"));
    }
    let transparency = match r.u8()? {
        0 => Transparency::Transparent,
        1 => Transparency::SemiOpaque,
        2 => Transparency::Opaque,
        // alloc: error path
        other => return Err(format!("transparency {other}")),
    };
    let shape = match r.u8()? {
        RULES_WITHOUT_TREE => Shape::RulesWithoutTree,
        RULES => Shape::Rules(r.tree()?),
        MACRO_WITHOUT_BODY => Shape::MacroWithoutBody,
        MACRO_BODY => Shape::Macro { args: None, body: r.tree()? },
        MACRO_ARGS_AND_BODY => {
            let args = r.tree()?;
            Shape::Macro { args: Some(args), body: r.tree()? }
        }
        // alloc: error path
        other => return Err(format!("definition shape {other}")),
    };
    if !r.bytes.is_empty() {
        // alloc: error path
        return Err(format!("{} bytes left over", r.bytes.len()));
    }
    Ok(Def { transparency, shape })
}

#[cfg(test)]
mod tests {
    use base_db::FileId;

    use super::*;

    /// A place whose text is one identifier of 64 bytes: every offset of a span below is in
    /// that token, and its squeeze is the identity.
    fn place(ast: u32) -> Place {
        place_of(ast, &"a".repeat(64))
    }

    /// The place of a definition `ast` whose text is `text`.
    fn place_of(ast: u32, text: &str) -> Place {
        let file = span::EditionedFileId::new(FileId::from_raw(7), Edition::Edition2021);
        Place {
            anchor: SpanAnchor { file_id: file, ast_id: ErasedFileAstId::from_raw(ast) },
            ctx: SyntaxContext::root(Edition::Edition2021),
            squeeze: Squeeze::of(text),
        }
    }

    fn span(place: &Place, start: u32, len: u32) -> Span {
        Span {
            range: TextRange::new(TextSize::new(start), TextSize::new(start + len)),
            anchor: place.anchor,
            ctx: place.ctx,
        }
    }

    /// `{ ($x:tt) => { [r#in, "s"suffix] } }` in shape, with every span of `spans`' place.
    fn tree(place: &Place, spans: &Place) -> tt::TopSubtree {
        let at = |start| span(spans, start, 1);
        let mut b = tt::TopSubtreeBuilder::new(tt::Delimiter {
            open: at(0),
            close: at(40),
            kind: tt::DelimiterKind::Brace,
        });
        b.open(tt::DelimiterKind::Parenthesis, at(1));
        b.push(tt::Leaf::Punct(tt::Punct { char: '$', spacing: tt::Spacing::JointHidden, span: at(2) }));
        b.push(tt::Leaf::Ident(tt::Ident::new("x", at(3))));
        b.close(at(4));
        b.push(tt::Leaf::Punct(tt::Punct { char: '=', spacing: tt::Spacing::Joint, span: at(5) }));
        b.push(tt::Leaf::Punct(tt::Punct { char: '>', spacing: tt::Spacing::Alone, span: at(6) }));
        b.open(tt::DelimiterKind::Bracket, at(8));
        b.push(tt::Leaf::Ident(tt::Ident::new("r#in", span(place, 9, 4))));
        b.push(tt::Leaf::Literal(tt::Literal::new("\"s\"", at(14), tt::LitKind::Str, "suffix")));
        b.close(at(30));
        b.build()
    }

    fn written(place: &Place, tt: &tt::TopSubtree) -> Option<Vec<u8>> {
        let mut w = Writer::new(place.clone());
        w.transparency(Transparency::Opaque);
        w.macro2(Some(tt), tt);
        w.finish()
    }

    /// The round trip: a definition read from its entry is the trees and transparency written.
    #[test]
    fn a_macro_definition_read_from_its_tokens_is_the_one_written() {
        let place = place(11);
        let tt = tree(&place, &place);
        let bytes = written(&place, &tt).expect("every span is the definition's own");
        let def = decode(&bytes, &place).expect("reads");
        assert_eq!(def.transparency, Transparency::Opaque);
        let Shape::Macro { args: Some(args), body } = &def.shape else {
            panic!("the shape written was args and a body: {def:?}");
        };
        assert_eq!((args, body), (&tt, &tt));
        // The same tokens are the same bytes, whatever wrote them.
        assert_eq!(written(&place, &tree(&place, &place)).as_deref(), Some(&*bytes));
    }

    /// The control of the round trip: a changed token is other bytes and reads as another tree.
    #[test]
    fn a_changed_token_is_another_entry() {
        let place = place(11);
        let tt = tree(&place, &place);
        let mut other = tree(&place, &place);
        other.set_token(2, tt::Leaf::Ident(tt::Ident::new("y", span(&place, 2, 1))));
        let (a, b) = (written(&place, &tt).unwrap(), written(&place, &other).unwrap());
        assert_ne!(a, b);
        assert_ne!(decode(&a, &place).unwrap(), decode(&b, &place).unwrap());
    }

    /// The invariant: the bytes are a function of the definition alone. A definition with a
    /// span of another anchor (its position among its siblings is in its ranges) is not kept.
    #[test]
    fn a_definition_with_a_span_that_is_not_its_own_is_not_kept() {
        let (own, wider) = (place(11), place(3));
        assert!(written(&own, &tree(&own, &own)).is_some());
        assert!(written(&own, &tree(&own, &wider)).is_none());
        let mut w = Writer::new(own.clone());
        w.rules(Some(&tree(&own, &own)));
        w.disown();
        assert!(w.finish().is_none());
    }

    /// A span that begins in whitespace or a comment is no token's: the squeezed text has no
    /// offset for it, so the definition is not kept. The control: the same span on a token is.
    #[test]
    fn a_span_in_whitespace_is_not_kept() {
        let place = place_of(11, "a  /* c */ b");
        let one = |start, len| {
            let mut b = tt::TopSubtreeBuilder::new(tt::Delimiter {
                open: span(&place, 0, 1),
                close: span(&place, 11, 1),
                kind: tt::DelimiterKind::Invisible,
            });
            b.push(tt::Leaf::Ident(tt::Ident::new("b", span(&place, start, len))));
            let mut w = Writer::new(place.clone());
            w.rules(Some(&b.build()));
            w.finish()
        };
        assert!(one(1, 1).is_none(), "a space");
        assert!(one(4, 1).is_none(), "a comment");
        assert!(one(11, 1).is_some(), "the control: `b`");
        // `a` to `b`, across the space and the comment, is kept and read back whole.
        let bytes = one(0, 12).expect("from a token to a token");
        let Shape::Rules(tt) = decode(&bytes, &place).expect("reads").shape else {
            panic!("rules were written");
        };
        let found = tt.as_token_trees().iter_flat_tokens().find_map(|it| match it {
            tt::TokenTree::Leaf(tt::Leaf::Ident(it)) => Some(it.span.range),
            _ => None,
        });
        assert_eq!(found, Some(TextRange::new(TextSize::new(0), TextSize::new(12))));
    }

    /// The text of a `macro_rules!` written in a file: `text` with its tree made as
    /// `decl_macro_expander` makes it, every span relative to the definition (its anchor), and
    /// the definition's place, of ast id 11 in whichever file; and its row's `header`, the
    /// content hash its tokens' key takes ([`def_hash`]).
    fn made(text: &str) -> (Place, TextSize, tt::TopSubtree, u128) {
        use syntax::{SourceFile, view::{self, AstNode as _, HasModuleItem as _}};

        /// Spans as a file's span map gives a definition that is its own anchor.
        struct Anchored {
            start: TextSize,
            anchor: SpanAnchor,
            ctx: SyntaxContext,
        }
        impl syntax_bridge::SpanMapper for Anchored {
            fn span_for(&self, range: TextRange) -> Span {
                Span { range: range - self.start, anchor: self.anchor, ctx: self.ctx }
            }
        }

        let parse = SourceFile::parse(text, Edition::Edition2021);
        let file = view::SourceFile::cast(parse.root()).expect("a file");
        let item = file.items().next().expect("an item");
        let header = index::item_source(item).header;
        let view::Item::MacroRules(rules) = item else {
            panic!("a `macro_rules!` first in {text:?}");
        };
        let range = rules.syntax().text_range();
        let place = place_of(11, &text[std::ops::Range::<usize>::from(range)]);
        let map = Anchored { start: range.start(), anchor: place.anchor, ctx: place.ctx };
        let call_site = syntax_bridge::SpanMapper::span_for(
            &map,
            rules.macro_rules_token().expect("the keyword").text_range(),
        );
        let tt = syntax_bridge::call_input(
            rules.token_tree().expect("a tree").syntax(),
            &map,
            call_site,
            syntax_bridge::DocCommentDesugarMode::Mbe,
        );
        (place, range.start(), tt, header)
    }

    fn rules_written(place: &Place, tt: &tt::TopSubtree) -> Vec<u8> {
        let mut w = Writer::new(place.clone());
        w.transparency(Transparency::SemiOpaque);
        w.rules(Some(tt));
        w.finish().expect("every span is the definition's own")
    }

    fn rules_read(bytes: &[u8], place: &Place) -> tt::TopSubtree {
        match decode(bytes, place).expect("reads").shape {
            Shape::Rules(tt) => tt,
            other => panic!("rules were written: {other:?}"),
        }
    }

    /// One definition as written, and the same tokens reindented, respaced, commented and
    /// moved down their file.
    const WRITTEN: &str = "macro_rules! m {\n    ($x:expr) => { boom($x) };\n}\n";
    const MOVED: &str = "// moved down\n\nmacro_rules! m\n{\n        ( $x : expr ) =>\n            \
                         { boom( $x ) } ; // a comment\n}\n";

    /// The invariant: two definitions of equal tokens, spaced and commented apart (one key:
    /// the row hashes tokens), are equal bytes, so no store holds two values under their
    /// key. The control: their trees as made from their text differ, in their spans alone,
    /// and those spans are what layout 2 wrote.
    #[test]
    fn two_definitions_of_equal_tokens_spaced_apart_are_equal_bytes() {
        let ((a_place, _, a, a_hash), (b_place, _, b, b_hash)) = (made(WRITTEN), made(MOVED));
        assert_eq!(a_hash, b_hash, "one key: the row hashes tokens");
        assert_eq!(rules_written(&a_place, &a), rules_written(&b_place, &b));
        assert_ne!(a, b, "the control: the spans as made differ");
        // Layout 2's bytes were those spans as they are.
        let raw = |tt: &tt::TopSubtree| {
            let mut out = Vec::new_in(Global);
            for token in tt.as_token_trees().iter_flat_tokens() {
                if let tt::TokenTree::Leaf(tt::Leaf::Ident(it)) = token {
                    out.push(it.span.range);
                }
            }
            out
        };
        assert_ne!(raw(&a), raw(&b), "the control: layout 2 held these and differed");
    }

    /// A definition moved and reindented is found by its key (the bytes its old text wrote)
    /// and read back to where its tokens are now: each tree read is the tree its own text
    /// makes, spans and all, so an error in an expansion points at the token where it is. The
    /// `boom` of the moved text, read from the bytes of the first, is at line 5, column 14.
    #[test]
    fn a_moved_definition_reads_back_to_where_its_tokens_are() {
        let ((a_place, a_start, a, _), (b_place, b_start, b, _)) = (made(WRITTEN), made(MOVED));
        let (a_bytes, b_bytes) = (rules_written(&a_place, &a), rules_written(&b_place, &b));
        assert_eq!(rules_read(&a_bytes, &b_place), b);
        assert_eq!(rules_read(&b_bytes, &a_place), a);

        let boom = |tt: &tt::TopSubtree, start: TextSize| {
            let range = tt
                .as_token_trees()
                .iter_flat_tokens()
                .find_map(|it| match it {
                    tt::TokenTree::Leaf(tt::Leaf::Ident(it)) if it.sym.as_str() == "boom" => {
                        Some(it.span.range)
                    }
                    _ => None,
                })
                .expect("`boom`");
            range + start
        };
        let at = boom(&rules_read(&a_bytes, &b_place), b_start);
        let offset = usize::from(at.start());
        assert_eq!(&MOVED[std::ops::Range::<usize>::from(at)], "boom");
        let line = MOVED[..offset].matches('\n').count();
        let column = offset - MOVED[..offset].rfind('\n').map_or(0, |it| it + 1);
        assert_eq!((line, column), (5, 14));
        // The control: where the first text has it, read from the bytes of the moved one.
        let at = boom(&rules_read(&b_bytes, &a_place), a_start);
        assert_eq!(&WRITTEN[std::ops::Range::<usize>::from(at)], "boom");
        let offset = usize::from(at.start());
        assert_eq!(WRITTEN[..offset].matches('\n').count(), 1);
    }

    /// A previous or foreign layout is refused by name, and bytes cut short say what is missing.
    #[test]
    fn a_previous_layout_of_macro_tokens_is_refused_by_name() {
        let place = place(11);
        let mut bytes = written(&place, &tree(&place, &place)).unwrap();
        let cut = decode(&bytes[..bytes.len() - 3], &place).unwrap_err();
        assert!(cut.contains("bytes asked of the"), "{cut}");
        bytes[..4].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
        let why = decode(&bytes, &place).unwrap_err();
        assert!(why.contains("macro def tokens of layout") && why.contains("this blaze reads"), "{why}");
    }

    /// Keys: the tokens' key moves with the definition's hash, its ast id and both editions,
    /// and with nothing of its file.
    #[test]
    fn the_tokens_key_covers_the_definition_and_how_it_is_read() {
        let ast = ErasedFileAstId::from_raw(11);
        let e21 = Edition::Edition2021;
        let key = tokens_key(1, ast, 2, e21, None);
        assert_eq!(key, tokens_key(1, ast, 2, e21, None));
        assert_ne!(key, tokens_key(2, ast, 2, e21, None));
        assert_ne!(key, tokens_key(1, ErasedFileAstId::from_raw(12), 2, e21, None));
        assert_ne!(key, tokens_key(1, ast, 3, e21, None));
        assert_ne!(key, tokens_key(1, ast, 2, Edition::Edition2018, None));
        // A definition whose text names `cfg` is keyed by its crate's cfg too: two cfgs, two
        // keys, and neither the key of the same text read as naming none.
        let (a, b) = (ContentId([1; 32]), ContentId([2; 32]));
        assert_ne!(key, tokens_key(1, ast, 2, e21, Some(&a)));
        assert_ne!(tokens_key(1, ast, 2, e21, Some(&a)), tokens_key(1, ast, 2, e21, Some(&b)));
    }
}
