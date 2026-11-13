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
//! ast id, so a span is two numbers and the reader gives the rest. A definition that is not
//! so (one a macro wrote, one whose spans have a wider anchor) is not kept and is made from
//! its source, as before. A definition whose text names `cfg` (a `cfg_attr` deciding its
//! transparency) is keyed by its crate's cfg too ([`cfg_part`]). Its content hash
//! ([`def_hash`]) is read from its expansion's index where a macro wrote it and the expansion
//! has a key ([`expansion`]).
//!
//! The invariant: equal keys are equal bytes; the tokens are a function of the definition's
//! tree and its ast id alone.

use std::sync::Arc;

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
/// definition whose transparency a `cfg_attr` decides is kept too.
pub const LAYOUT: u32 = 2;

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
    let set = file_set(db, file);
    let def_edition = def_crate.data(db).edition;
    let cfg = cfg_part(db, file, ast, def_crate);
    let key = tokens_key(hash, ast, read_as(db, file), def_edition, cfg.as_ref());
    let place = Place::of(db, file, ast);
    let mut def = None;
    kept.get(db, def_crate, TOKENS, &set, key, &mut |bytes| {
        def = Some(decode(bytes, place)?);
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
/// the file's root context.
#[derive(Clone, Copy)]
pub(super) struct Place {
    anchor: SpanAnchor,
    ctx: SyntaxContext,
}

impl Place {
    pub(super) fn of(db: &Db, file: EditionedFileId, ast: ErasedFileAstId) -> Place {
        Place {
            anchor: SpanAnchor { file_id: file.span_file_id(db), ast_id: ast },
            ctx: SyntaxContext::root(file.edition(db)),
        }
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

    fn span(&mut self, it: Span) {
        if it.anchor != self.place.anchor || it.ctx != self.place.ctx {
            self.own = false;
        }
        self.u32(it.range.start().into());
        self.u32(it.range.len().into());
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
    place: Place,
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

    fn span(&mut self) -> R<Span> {
        let start = self.u32()?;
        let len = self.u32()?;
        let end = start.checked_add(len).ok_or("a span past 4 GiB")?;
        Ok(Span {
            range: TextRange::new(TextSize::new(start), TextSize::new(end)),
            anchor: self.place.anchor,
            ctx: self.place.ctx,
        })
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
pub(super) fn decode(bytes: &[u8], place: Place) -> R<Def> {
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

    fn place(ast: u32) -> Place {
        let file = span::EditionedFileId::new(FileId::from_raw(7), Edition::Edition2021);
        Place {
            anchor: SpanAnchor { file_id: file, ast_id: ErasedFileAstId::from_raw(ast) },
            ctx: SyntaxContext::root(Edition::Edition2021),
        }
    }

    fn span(place: Place, start: u32, len: u32) -> Span {
        Span {
            range: TextRange::new(TextSize::new(start), TextSize::new(start + len)),
            anchor: place.anchor,
            ctx: place.ctx,
        }
    }

    /// `{ ($x:tt) => { [r#in, "s"suffix] } }` in shape, with every span of `spans`' place.
    fn tree(place: Place, spans: Place) -> tt::TopSubtree {
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

    fn written(place: Place, tt: &tt::TopSubtree) -> Option<Vec<u8>> {
        let mut w = Writer::new(place);
        w.transparency(Transparency::Opaque);
        w.macro2(Some(tt), tt);
        w.finish()
    }

    /// The round trip: a definition read from its entry is the trees and transparency written.
    #[test]
    fn a_macro_definition_read_from_its_tokens_is_the_one_written() {
        let place = place(11);
        let tt = tree(place, place);
        let bytes = written(place, &tt).expect("every span is the definition's own");
        let def = decode(&bytes, place).expect("reads");
        assert_eq!(def.transparency, Transparency::Opaque);
        let Shape::Macro { args: Some(args), body } = &def.shape else {
            panic!("the shape written was args and a body: {def:?}");
        };
        assert_eq!((args, body), (&tt, &tt));
        // The same tokens are the same bytes, whatever wrote them.
        assert_eq!(written(place, &tree(place, place)).as_deref(), Some(&*bytes));
    }

    /// The control of the round trip: a changed token is other bytes and reads as another tree.
    #[test]
    fn a_changed_token_is_another_entry() {
        let place = place(11);
        let tt = tree(place, place);
        let mut other = tree(place, place);
        other.set_token(2, tt::Leaf::Ident(tt::Ident::new("y", span(place, 2, 1))));
        let (a, b) = (written(place, &tt).unwrap(), written(place, &other).unwrap());
        assert_ne!(a, b);
        assert_ne!(decode(&a, place).unwrap(), decode(&b, place).unwrap());
    }

    /// The invariant: the bytes are a function of the definition alone. A definition with a
    /// span of another anchor (its position among its siblings is in its ranges) is not kept.
    #[test]
    fn a_definition_with_a_span_that_is_not_its_own_is_not_kept() {
        let (own, wider) = (place(11), place(3));
        assert!(written(own, &tree(own, own)).is_some());
        assert!(written(own, &tree(own, wider)).is_none());
        let mut w = Writer::new(own);
        w.rules(Some(&tree(own, own)));
        w.disown();
        assert!(w.finish().is_none());
    }

    /// A previous or foreign layout is refused by name, and bytes cut short say what is missing.
    #[test]
    fn a_previous_layout_of_macro_tokens_is_refused_by_name() {
        let place = place(11);
        let mut bytes = written(place, &tree(place, place)).unwrap();
        let cut = decode(&bytes[..bytes.len() - 3], place).unwrap_err();
        assert!(cut.contains("bytes asked of the"), "{cut}");
        bytes[..4].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
        let why = decode(&bytes, place).unwrap_err();
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
