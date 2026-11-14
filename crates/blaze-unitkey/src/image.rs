//! A file's `FileImage`: parsed once, walked once, written as plain tables.
//!
//! Per item row it gives, read in place ([`crate::read::Image`]): a node's header digest (its
//! tokens with bodies cut, its piece of the surface), the item's own-text digest (header and
//! body, `BodyInputs::text`), the digest of each item written in its body that escapes it
//! ([`escapes`]), whether its body holds a macro that is not quiet ([`CALLS`]), its late reads,
//! its `#[cfg]`, and its tokens' and its body's ranges (what a body task finds its syntax by).
//!
//! Tokens are hashed as (kind, joint, length-prefixed text): `a b` and `ab` differ, `x - -y`
//! and `x --y` differ ([`JOINT`]), a doc comment is a token (an attribute) and a string
//! literal's text is fed whole. Plain comments and whitespace are in no digest but the file
//! image's own key, which is its bytes.
//!
//! Cost: one parse, one walk over the tree's items, and each token hashed at most three times
//! (in its node's header or an escape's, in its item's own text, scanned once in its body).
//! Floor: the file's one reparse; [`key`] is all an unchanged file costs.

#![blaze::sans_io]

use core::{
    alloc::Allocator,
    mem::{offset_of, size_of},
};
use std::alloc::Global;

use blaze_format::{Record, Slice, Writer};
use blaze_ids::{ContentId, Hasher};
use blaze_keyed::{
    kind::{FileImage, FileInputs, Key, Kind, ToolId},
    record::{RecordHead, Section, Sections},
};
use cfg::CfgExpr;
use syntax::{
    Edition, Extensions, SourceFile, SyntaxKind, WalkEvent,
    view::{self, AstNode, AstToken},
};

use crate::{
    ImageError, cfgs, quiet,
    rows::{
        self, BODY, CALLS, IN_DEFINITION, ImageTables, ItemRow, JOINT, LAYOUT, LateRow, ModRow, NODE, NONE, PathRow,
        Span, Tok, kind, section,
    },
};

/// What a file is parsed under: its crate's edition and syntax extensions. Both decide the tree,
/// so both are in the file image's key, as one word in `FileInputs::edition` (the edition in the
/// low byte, `Extensions::bits` above it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reading {
    pub edition: Edition,
    pub extensions: Extensions,
}

impl Reading {
    pub fn word(self) -> u16 {
        self.edition as u16 | u16::from(self.extensions.bits()) << 8
    }

    pub fn from_word(word: u16) -> Result<Reading, ImageError> {
        let edition = match word & 0xff {
            0 => Edition::Edition2015,
            1 => Edition::Edition2018,
            2 => Edition::Edition2021,
            3 => Edition::Edition2024,
            _ => return Err(ImageError::Reading { word }),
        };
        let extensions = Extensions::from_bits((word >> 8) as u8).ok_or(ImageError::Reading { word })?;
        Ok(Reading { edition, extensions })
    }
}

/// The file image's key: the toolchain, the reading and the bytes. What an unchanged file costs:
/// this hash and one store lookup.
pub fn key(tool: &ToolId, reading: Reading, bytes: &[u8]) -> Key<FileImage> {
    FileImage::key(&FileInputs { tool: *tool, edition: reading.word(), bytes })
}

/// A written file image: its key and its bytes (the store files them; a reader maps them
/// 16-aligned).
pub struct Imaged {
    pub key: Key<FileImage>,
    pub record: Vec<u8>,
}

/// The file image of `bytes`, parsed under `reading`. Scratch is in `frame`; the record is one
/// `Vec` of the global heap, the store's to file.
pub fn image<A: Allocator + Clone>(
    tool: &ToolId,
    reading: Reading,
    bytes: &[u8],
    frame: A,
) -> Result<Imaged, ImageError> {
    let text = core::str::from_utf8(bytes).map_err(|_| ImageError::NotText)?;
    if u32::try_from(text.len()).is_err() {
        return Err(ImageError::TooLarge { what: "a source file's length" });
    }
    let key = key(tool, reading, bytes);
    let parse = SourceFile::parse_shared_with(text, reading.edition, reading.extensions);
    let root = parse.root();
    let mut walk = Walk::new(text, frame.clone());
    walk.lex(root)?;
    let file_cfg = walk.file(root)?;
    walk.dedup_paths();
    let mut tree = Vec::new_in(Global);
    let range = syntax::image::write(&parse, &mut tree);
    let mut lines = Vec::new_in(frame);
    lines.push(0u32);
    for (at, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(at as u32 + 1);
        }
    }
    let record = walk.write(&key, &lines, &tree[range], file_cfg, reading.word());
    Ok(Imaged { key, record })
}

/// A run's mark in a header digest: the item's first run.
const START: u8 = 0;
/// A run after a cut.
const CUT: u8 = 1;

/// What an item's attributes say ([`attr_flags`]).
/// An attribute macro (an attribute that is not inert): its expansion reads the whole item.
const A_MACRO: u32 = 1;
const A_MACRO_EXPORT: u32 = 1 << 1;
/// `#[no_mangle]`, `#[export_name]`, `#[used]`, `#[link_section]`.
const A_EXPORT: u32 = 1 << 2;
const A_LINK: u32 = 1 << 3;
/// `#[derive]`: its expansion makes impls no text shows.
const A_DERIVE: u32 = 1 << 4;

/// An item's attributes: their flags, its `#[cfg]` and its `#[path]`s.
struct Attrs {
    flags: u32,
    cfg: Span,
    paths: Span,
}

/// One item's body, hashed and scanned.
struct BodyFacts {
    text: ContentId,
    escapes: Span,
    tokens: Span,
    calls: bool,
}

struct Walk<'t, A: Allocator + Clone> {
    text: &'t str,
    frame: A,
    tokens: Vec<Tok, A>,
    views: Vec<view::Token<'t>, A>,
    rows: Vec<ItemRow, A>,
    escapes: Vec<ContentId, A>,
    strs: Vec<u8, A>,
    cfgs: Vec<u8, A>,
    lates: Vec<LateRow, A>,
    mods: Vec<ModRow, A>,
    paths: Vec<PathRow, A>,
    /// Scratch: the late reads of the row being made, cleared by each row.
    late_rows: Vec<LateRow, A>,
    cfg_tmp: Vec<u8, A>,
}

fn strip_raw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
}

fn is_open(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::L_PAREN | SyntaxKind::L_BRACK | SyntaxKind::L_CURLY)
}

fn is_close(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::R_PAREN | SyntaxKind::R_BRACK | SyntaxKind::R_CURLY)
}

/// Not hashed, not a token of the image: whitespace, plain comments, the shebang. A doc comment
/// is a token: it is an attribute.
fn is_skipped(kind: SyntaxKind) -> bool {
    kind.is_trivia() || matches!(kind, SyntaxKind::NEWLINE | SyntaxKind::SHEBANG)
}

/// The item node kinds and the row kind of each.
const ITEM_KINDS: [(SyntaxKind, u32); 17] = [
    (SyntaxKind::FN, kind::FN),
    (SyntaxKind::CONST, kind::CONST),
    (SyntaxKind::STATIC, kind::STATIC),
    (SyntaxKind::STRUCT, kind::STRUCT),
    (SyntaxKind::ENUM, kind::ENUM),
    (SyntaxKind::UNION, kind::UNION),
    (SyntaxKind::TRAIT, kind::TRAIT),
    (SyntaxKind::IMPL, kind::IMPL),
    (SyntaxKind::TYPE_ALIAS, kind::ALIAS),
    (SyntaxKind::MODULE, kind::MODULE),
    (SyntaxKind::USE, kind::USE),
    (SyntaxKind::EXTERN_CRATE, kind::EXTERN_CRATE),
    (SyntaxKind::EXTERN_BLOCK, kind::EXTERN_BLOCK),
    (SyntaxKind::MACRO_CALL, kind::MACRO_CALL),
    (SyntaxKind::MACRO_RULES, kind::MACRO_RULES),
    (SyntaxKind::MACRO_DEF, kind::MACRO_DEF),
    // Global `asm!` at item level.
    (SyntaxKind::ASM_EXPR, kind::OTHER),
];

/// An item's kind, by its node's.
fn item_kind(node: SyntaxKind) -> Option<u32> {
    ITEM_KINDS.iter().find(|(syntax, _)| *syntax == node).map(|(_, row)| *row)
}

/// The kinds whose body is keyed: what follows `=` of a const or static, a fn's block.
fn is_bodied(node: SyntaxKind) -> bool {
    matches!(node, SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC)
}

/// The body of a fn, const or static, if it has one.
fn body_of(item: view::Node<'_>) -> Option<view::Node<'_>> {
    if item.kind() == SyntaxKind::FN {
        return item.children().find(|child| child.kind() == SyntaxKind::BLOCK_EXPR);
    }
    if !matches!(item.kind(), SyntaxKind::CONST | SyntaxKind::STATIC) {
        return None;
    }
    // The expression is the first child node after the `=`.
    let eq = item.child_token(SyntaxKind::EQ)?;
    let at = eq.text_range().end();
    item.children().find(|child| child.text_range().start() >= at)
}

/// The body of `item` when it is a fn, const or static with one.
fn own_body(item: view::Node<'_>) -> Option<view::Node<'_>> {
    match is_bodied(item.kind()) {
        true => body_of(item),
        false => None,
    }
}

/// The name an item is declared as, `r#` cut.
fn name_text<'t>(item: view::Node<'t>) -> Option<&'t str> {
    let name = item.children().find(|child| child.kind() == SyntaxKind::NAME)?;
    name.first_token().map(|token| strip_raw(token.text()))
}

/// An attribute's path: its first segment and whether that is all of it.
fn path_head<'t>(path: view::Path<'t>) -> (&'t str, bool) {
    let (mut first, mut whole, mut seen) = ("", true, false);
    for element in path.syntax().descendants_with_tokens() {
        let Some(token) = element.into_token() else { continue };
        if is_skipped(token.kind()) {
            continue;
        }
        if token.kind() == SyntaxKind::COLON2 {
            whole = false;
        } else if !seen {
            (first, seen) = (strip_raw(token.text()), true);
        }
    }
    (first, whole)
}

impl<'t, A: Allocator + Clone> Walk<'t, A> {
    fn new(text: &'t str, frame: A) -> Walk<'t, A> {
        Walk {
            text,
            tokens: Vec::new_in(frame.clone()),
            views: Vec::new_in(frame.clone()),
            rows: Vec::new_in(frame.clone()),
            escapes: Vec::new_in(frame.clone()),
            strs: Vec::new_in(frame.clone()),
            cfgs: Vec::new_in(frame.clone()),
            lates: Vec::new_in(frame.clone()),
            mods: Vec::new_in(frame.clone()),
            paths: Vec::new_in(frame.clone()),
            late_rows: Vec::new_in(frame.clone()),
            cfg_tmp: Vec::new_in(frame.clone()),
            frame,
        }
    }

    /// The token table: every token but trivia, with its flags.
    fn lex(&mut self, root: view::Node<'t>) -> Result<(), ImageError> {
        for element in root.descendants_with_tokens() {
            let Some(token) = element.into_token() else { continue };
            if is_skipped(token.kind()) {
                continue;
            }
            let range = token.text_range();
            self.tokens.push(Tok {
                start: u32::from(range.start()),
                len: u32::from(range.len()),
                kind: u16::from(token.kind()),
                flags: 0,
                pad: 0,
            });
            self.views.push(token);
        }
        if u32::try_from(self.tokens.len()).is_err() {
            return Err(ImageError::TooLarge { what: "a file's tokens" });
        }
        // Joint: a punctuation token the next punctuation token follows with nothing between.
        for at in 0..self.tokens.len() {
            let this = self.tokens[at];
            let joint = SyntaxKind::from(this.kind).is_punct()
                && self.tokens.get(at + 1).is_some_and(|next| {
                    SyntaxKind::from(next.kind).is_punct() && next.start == this.start + this.len
                });
            if joint {
                self.tokens[at].flags |= JOINT;
            }
        }
        for node in root.descendants() {
            if matches!(node.kind(), SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF) {
                let span = self.span(node);
                for token in &mut self.tokens[span.range()] {
                    token.flags |= IN_DEFINITION;
                }
            }
        }
        Ok(())
    }

    /// The tokens of `node`, as a span of the token table.
    fn span(&self, node: view::Node<'_>) -> Span {
        let range = node.text_range();
        let (start, end) = (u32::from(range.start()), u32::from(range.end()));
        let from = self.tokens.partition_point(|token| token.start < start);
        let to = self.tokens.partition_point(|token| token.start < end);
        Span { at: from as u32, len: (to - from) as u32 }
    }

    fn token_text(&self, at: usize) -> &'t str {
        let token = self.tokens[at];
        &self.text[token.start as usize..(token.start + token.len) as usize]
    }

    fn kind_at(&self, at: usize) -> SyntaxKind {
        SyntaxKind::from(self.tokens[at].kind)
    }

    /// Pushes `text` to the string pool.
    fn push_str(&mut self, text: &[u8]) -> Span {
        let at = self.strs.len() as u32;
        self.strs.extend_from_slice(text);
        Span { at, len: text.len() as u32 }
    }

    /// The in-file path of `item`: `prefix::name`, or its header in braces for an item without a
    /// name of its own.
    fn item_path(&mut self, item: view::Node<'t>, kind: u32, prefix: Span) -> Span {
        let at = self.strs.len() as u32;
        if prefix.len > 0 {
            self.strs.extend_from_within(prefix.range());
            self.strs.extend_from_slice(b"::");
        }
        match kind {
            kind::IMPL => self.braced(item, b"{impl", SyntaxKind::ASSOC_ITEM_LIST),
            kind::USE => self.braced(item, b"{use", SyntaxKind::USE),
            kind::EXTERN_BLOCK => self.braced(item, b"{extern", SyntaxKind::EXTERN_ITEM_LIST),
            kind::MACRO_CALL => self.braced(item, b"{call", SyntaxKind::TOKEN_TREE),
            kind::OTHER => self.strs.extend_from_slice(b"{other}"),
            kind::INNER_ATTRS => self.strs.extend_from_slice(b"{inner}"),
            _ => {
                let name = name_text(item).unwrap_or("{unnamed}");
                self.strs.extend_from_slice(name.as_bytes());
            }
        }
        Span { at, len: self.strs.len() as u32 - at }
    }

    /// `{word <tokens of item up to stop>}`: every token of the item but its attributes and doc
    /// comments, before its first child of kind `stop`.
    fn braced(&mut self, item: view::Node<'t>, word: &[u8], stop: SyntaxKind) {
        self.strs.extend_from_slice(word);
        let span = self.span(item);
        let stop_at = item
            .children()
            .find(|child| child.kind() == stop)
            .map_or(span.at + span.len, |child| self.span(child).at);
        let mut attrs: Vec<Span, A> = Vec::new_in(self.frame.clone());
        for child in item.children() {
            if child.kind() == SyntaxKind::ATTR {
                attrs.push(self.span(child));
            }
        }
        for at in span.at..stop_at {
            if attrs.iter().any(|attr| attr.range().contains(&(at as usize))) {
                continue;
            }
            if matches!(
                self.kind_at(at as usize),
                SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::DOC_COMMENT
            ) {
                continue;
            }
            self.strs.push(b' ');
            let text = self.token_text(at as usize);
            self.strs.extend_from_slice(text.as_bytes());
        }
        self.strs.push(b'}');
    }

    /// The file's rows. Returns its inner `#![cfg]`.
    ///
    /// The tokens at the file's top level that are in no item (its inner attributes, its inner
    /// doc comments, a stray token) are one node, `{inner}`: its header is their digest.
    fn file(&mut self, root: view::Node<'t>) -> Result<Span, ImageError> {
        let inner = self.push_str(b"{inner}");
        let attrs = self.attrs(root, inner)?;
        let whole = self.span(root);
        let mut cuts: Vec<Span, A> = Vec::new_in(self.frame.clone());
        for child in root.children() {
            if child.kind() != SyntaxKind::ATTR {
                cuts.push(self.span(child));
            }
        }
        let cut: u32 = cuts.iter().map(|span| span.len).sum();
        if cut < whole.len {
            self.late_rows.clear();
            let header = self.hash_runs(whole, &cuts, true);
            let lates = self.pool_lates();
            self.rows.push(ItemRow {
                header,
                text: ContentId([0; 32]),
                escapes: Span::EMPTY,
                path: inner,
                cfg: Span::EMPTY,
                lates,
                tokens: whole,
                body_tokens: Span::EMPTY,
                parent: NONE,
                kind: kind::INNER_ATTRS,
                flags: NODE,
                pad: 0,
            });
        }
        self.items(root, NONE, Span::EMPTY, NONE)?;
        Ok(attrs.cfg)
    }

    /// The items of `list` (a file, an inline module's item list).
    fn items(&mut self, list: view::Node<'t>, parent: u32, prefix: Span, mod_parent: u32) -> Result<(), ImageError> {
        for child in list.children() {
            if child.kind() == SyntaxKind::ATTR {
                continue;
            }
            // A node that is not an item where items are (a parse error) is a node of its own,
            // so its tokens are in some piece.
            let kind = item_kind(child.kind()).unwrap_or(kind::OTHER);
            self.node(child, kind, parent, prefix, mod_parent)?;
        }
        Ok(())
    }

    /// One node row, then its associated rows, then an inline module's items.
    fn node(
        &mut self,
        item: view::Node<'t>,
        kind: u32,
        parent: u32,
        prefix: Span,
        mod_parent: u32,
    ) -> Result<(), ImageError> {
        let path = self.item_path(item, kind, prefix);
        let attrs = self.attrs(item, path)?;
        self.late_rows.clear();
        let mut bodies: Vec<view::Node<'t>, A> = Vec::new_in(self.frame.clone());
        let header = self.header(item, attrs.flags, true, &mut bodies);
        let mut row = ItemRow {
            header,
            text: ContentId([0; 32]),
            escapes: Span::EMPTY,
            path,
            cfg: attrs.cfg,
            lates: Span::EMPTY,
            tokens: self.span(item),
            body_tokens: Span::EMPTY,
            parent,
            kind,
            flags: NODE,
            pad: 0,
        };
        if let Some(body) = own_body(item) {
            let facts = self.body(item, body);
            row.text = facts.text;
            row.escapes = facts.escapes;
            row.body_tokens = facts.tokens;
            row.flags |= BODY;
            if facts.calls {
                row.flags |= CALLS;
            }
        }
        row.lates = self.pool_lates();
        let at = self.rows.len() as u32;
        self.rows.push(row);
        match kind {
            kind::IMPL | kind::TRAIT => self.assoc(item, at, path)?,
            kind::MODULE => self.module(item, at, path, attrs.paths, mod_parent)?,
            _ => {}
        }
        Ok(())
    }

    /// A container's associated items with a body: a row each, right after the container's.
    /// Every associated item's attributes are read, so a `#[cfg]` that does not parse is refused
    /// wherever it is.
    fn assoc(&mut self, container: view::Node<'t>, row: u32, path: Span) -> Result<(), ImageError> {
        let Some(list) = container.children().find(|child| child.kind() == SyntaxKind::ASSOC_ITEM_LIST) else {
            return Ok(());
        };
        for item in list.children() {
            if item.kind() == SyntaxKind::ATTR {
                continue;
            }
            let Some(kind) = item_kind(item.kind()) else { continue };
            if kind == kind::MACRO_CALL {
                continue;
            }
            let item_path = self.item_path(item, kind, path);
            let attrs = self.attrs(item, item_path)?;
            let Some(body) = own_body(item) else { continue };
            self.late_rows.clear();
            let facts = self.body(item, body);
            let mut flags = BODY;
            if facts.calls {
                flags |= CALLS;
            }
            let lates = self.pool_lates();
            self.rows.push(ItemRow {
                header: ContentId([0; 32]),
                text: facts.text,
                escapes: facts.escapes,
                path: item_path,
                cfg: attrs.cfg,
                lates,
                tokens: self.span(item),
                body_tokens: facts.tokens,
                parent: row,
                kind,
                flags,
                pad: 0,
            });
        }
        Ok(())
    }

    /// A module declaration's mod row, and an inline module's items.
    fn module(&mut self, item: view::Node<'t>, row: u32, path: Span, paths: Span, mod_parent: u32) -> Result<(), ImageError> {
        let list = item.children().find(|child| child.kind() == SyntaxKind::ITEM_LIST);
        let name = name_text(item).unwrap_or("");
        let name = self.push_str(name.as_bytes());
        let at = self.mods.len() as u32;
        self.mods.push(ModRow {
            name,
            paths,
            row,
            parent: mod_parent,
            flags: match list {
                Some(_) => rows::INLINE,
                None => 0,
            },
            pad: 0,
        });
        if let Some(list) = list {
            self.items(list, row, path, at)?;
        }
        Ok(())
    }

    /// The header digest of `item`: its tokens with its bodies cut (its own, and its associated
    /// items' when it is an impl or a trait) and its inline module's items cut (they are rows of
    /// their own). An item under an attribute macro ([`A_MACRO`] in `flags`) cuts nothing: the
    /// macro reads all of it, and so does the surface that expands it; likewise an associated
    /// item under one keeps its body. Only the item's own bodies are cut: an expression inside
    /// a header (an array length, a const argument) stays, whatever it holds. The cut bodies are
    /// appended to `bodies`. With `lates`, the runs' late reads go to `late_rows`.
    fn header(&mut self, item: view::Node<'t>, flags: u32, lates: bool, bodies: &mut Vec<view::Node<'t>, A>) -> ContentId {
        let first = bodies.len();
        let mut cuts: Vec<Span, A> = Vec::new_in(self.frame.clone());
        if flags & A_MACRO == 0 {
            if let Some(body) = own_body(item) {
                bodies.push(body);
            }
            match item.kind() {
                SyntaxKind::IMPL | SyntaxKind::TRAIT => {
                    if let Some(list) = item.children().find(|child| child.kind() == SyntaxKind::ASSOC_ITEM_LIST) {
                        for assoc in list.children() {
                            if own_flags(assoc) & A_MACRO == 0
                                && let Some(body) = own_body(assoc)
                            {
                                bodies.push(body);
                            }
                        }
                    }
                }
                SyntaxKind::MODULE => {
                    if let Some(list) = item.children().find(|child| child.kind() == SyntaxKind::ITEM_LIST) {
                        for child in list.children() {
                            if child.kind() != SyntaxKind::ATTR {
                                cuts.push(self.span(child));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for at in first..bodies.len() {
            let span = self.span(bodies[at]);
            cuts.push(span);
        }
        cuts.sort_unstable_by_key(|span| span.at);
        let whole = self.span(item);
        self.hash_runs(whole, &cuts, lates)
    }

    /// The digest of `whole` with `cuts` (sorted, disjoint) taken out: each run with its mark
    /// and its length, then its tokens. With `lates`, each run is scanned for late reads.
    fn hash_runs(&mut self, whole: Span, cuts: &[Span], lates: bool) -> ContentId {
        let mut hasher = Hasher::new("blaze-unitkey header");
        let end = whole.at + whole.len;
        let mut at = whole.at;
        let mut mark = START;
        for cut in cuts {
            let from = cut.at.max(at);
            self.run(&mut hasher, at, from, mark, lates);
            at = (cut.at + cut.len).max(at);
            mark = CUT;
        }
        self.run(&mut hasher, at, end.max(at), mark, lates);
        hasher.finish()
    }

    fn run(&mut self, hasher: &mut Hasher, from: u32, to: u32, mark: u8, lates: bool) {
        hasher.update(&[mark]).update(&(to - from).to_le_bytes());
        for token in from..to {
            self.hash_token(hasher, token as usize);
        }
        if lates {
            self.scan(from, to);
        }
    }

    /// A token as (kind, joint, length-prefixed text).
    fn hash_token(&self, hasher: &mut Hasher, at: usize) {
        let token = self.tokens[at];
        hasher.update(&token.kind.to_le_bytes()).update(&[token.flags & JOINT]).part(self.token_text(at).as_bytes());
    }

    /// Scans tokens `from..to`: each late read into `late_rows`. Returns whether a macro call
    /// not on the quiet list is among them.
    fn scan(&mut self, from: u32, to: u32) -> bool {
        let mut calls = false;
        let to = to as usize;
        for at in from as usize..to {
            if self.kind_at(at) != SyntaxKind::IDENT {
                continue;
            }
            let name = strip_raw(self.token_text(at));
            let call = self.tokens[at].flags & IN_DEFINITION == 0
                && at + 2 < to
                && self.kind_at(at + 1) == SyntaxKind::BANG
                && is_open(self.kind_at(at + 2))
                && name != "macro_rules";
            if call {
                if let Some(late) = quiet::late_of(name.as_bytes()) {
                    self.late(at, to, late as u8);
                }
                if !quiet::is_quiet(name.as_bytes()) {
                    calls = true;
                }
            }
        }
        calls
    }

    /// The late read of the call whose name is token `at` (`name ! ( args )`), into `late_rows`.
    /// A first argument that is a string literal is unescaped; any other argument is given as
    /// its tokens' text, for the caller to evaluate (`concat!(env!("A"), "/b")`).
    fn late(&mut self, at: usize, to: usize, late: u8) {
        let open = at + 2;
        let mut depth = 0u32;
        let mut close = None;
        for index in open..to {
            let kind = self.kind_at(index);
            if is_open(kind) {
                depth += 1;
            } else if is_close(kind) {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
        }
        // A call whose arguments do not close in this run is not a call the parser made.
        let Some(close) = close else { return };
        let first = open + 1;
        let literal = first < close
            && self.kind_at(first) == SyntaxKind::STRING
            && (first + 1 == close || self.kind_at(first + 1) == SyntaxKind::COMMA);
        let value = match literal {
            true => view::String::cast(self.views[first]).and_then(|string| string.value().ok()),
            false => None,
        };
        let (name, literal) = match value {
            Some(value) => (self.push_str(value.as_bytes()), 1),
            None => {
                let start = self.strs.len() as u32;
                for index in first..close {
                    if index > first {
                        self.strs.push(b' ');
                    }
                    let text = self.token_text(index);
                    self.strs.extend_from_slice(text.as_bytes());
                }
                (Span { at: start, len: self.strs.len() as u32 - start }, 0)
            }
        };
        self.late_rows.push(LateRow { name, token: at as u32, late, literal, pad: 0 });
    }

    /// `late_rows`, in token order, into the late pool.
    fn pool_lates(&mut self) -> Span {
        let at = self.lates.len() as u32;
        self.lates.extend_from_slice(&self.late_rows);
        let span = Span { at, len: self.late_rows.len() as u32 };
        self.late_rows.clear();
        span
    }

    /// The body of `item`: the item's own-text digest, the digests of what escapes the body,
    /// the body's token span, and whether it holds a macro that is not quiet. Its late reads go
    /// to `late_rows`.
    fn body(&mut self, item: view::Node<'t>, body: view::Node<'t>) -> BodyFacts {
        let tokens = self.span(item);
        let mut hasher = Hasher::new("blaze-unitkey own text");
        hasher.update(&tokens.len.to_le_bytes());
        for at in tokens.range() {
            self.hash_token(&mut hasher, at);
        }
        let span = self.span(body);
        let mut calls = self.scan(span.at, span.at + span.len);
        calls |= body.descendants().any(|node| {
            node != body && item_kind(node.kind()).is_some() && own_flags(node) & (A_MACRO | A_DERIVE) != 0
        });
        let escapes = self.escapes_of(body);
        BodyFacts { text: hasher.finish(), escapes, tokens: span, calls }
    }

    /// The digests of the items in `body` that escape it ([`escapes`]), into the escape pool: each
    /// one's header digest, then those escaping its own cut bodies, in the order found. The work
    /// list only grows by items strictly inside one taken from it, so it ends.
    fn escapes_of(&mut self, body: view::Node<'t>) -> Span {
        let at = self.escapes.len() as u32;
        let mut queue: Vec<view::Node<'t>, A> = Vec::new_in(self.frame.clone());
        find_escapes(body, &mut queue);
        let mut bodies: Vec<view::Node<'t>, A> = Vec::new_in(self.frame.clone());
        let mut done = 0;
        while done < queue.len() {
            let escape = queue[done];
            done += 1;
            bodies.clear();
            let digest = self.header(escape, own_flags(escape), false, &mut bodies);
            self.escapes.push(digest);
            for inner in &bodies {
                find_escapes(*inner, &mut queue);
            }
        }
        Span { at, len: self.escapes.len() as u32 - at }
    }

    /// The item's attributes: their flags, its `#[cfg]` into the cfg pool, its `#[path]`s into
    /// the path table. `item_path` names the item in an error.
    fn attrs(&mut self, item: view::Node<'t>, item_path: Span) -> Result<Attrs, ImageError> {
        let mut flags = 0;
        let mut count = 0u32;
        self.cfg_tmp.clear();
        let paths_at = self.paths.len() as u32;
        let mut conds: Vec<view::CfgPredicate<'t>, A> = Vec::new_in(self.frame.clone());
        for child in item.children() {
            if child.kind() != SyntaxKind::ATTR {
                continue;
            }
            let Some(meta) = view::Attr::cast(child).and_then(|attr| attr.meta()) else { continue };
            self.meta(meta, &mut conds, &mut flags, &mut count, item_path)?;
        }
        let cfg = match count {
            0 => Span::EMPTY,
            _ => {
                let at = self.cfgs.len() as u32;
                if count > 1 {
                    let count = u16::try_from(count).map_err(|_| self.cfg_error(item_path, cfgs::CfgRefused::TooLarge))?;
                    cfgs::all_head(count, &mut self.cfgs);
                }
                self.cfgs.extend_from_slice(&self.cfg_tmp);
                Span { at, len: self.cfgs.len() as u32 - at }
            }
        };
        let paths = Span { at: paths_at, len: self.paths.len() as u32 - paths_at };
        Ok(Attrs { flags, cfg, paths })
    }

    fn cfg_error(&self, item_path: Span, refused: cfgs::CfgRefused) -> ImageError {
        let mut item = Vec::new_in(Global);
        item.extend_from_slice(&self.strs[item_path.range()]);
        ImageError::Cfg { item, rule: refused.rule() }
    }

    /// One meta of an attribute, under the `cfg_attr` conditions `conds`.
    fn meta(
        &mut self,
        meta: view::Meta<'t>,
        conds: &mut Vec<view::CfgPredicate<'t>, A>,
        flags: &mut u32,
        count: &mut u32,
        item_path: Span,
    ) -> Result<(), ImageError> {
        match meta {
            view::Meta::CfgMeta(cfg) => {
                let Some(predicate) = cfg.cfg_predicate() else {
                    return Err(self.cfg_error(item_path, cfgs::CfgRefused::Invalid));
                };
                let mut out = core::mem::replace(&mut self.cfg_tmp, Vec::new_in(self.frame.clone()));
                let written = implied(conds, predicate, &mut out);
                self.cfg_tmp = out;
                written.map_err(|refused| self.cfg_error(item_path, refused))?;
                *count += 1;
            }
            view::Meta::CfgAttrMeta(cfg_attr) => {
                let Some(predicate) = cfg_attr.cfg_predicate() else {
                    return Err(self.cfg_error(item_path, cfgs::CfgRefused::Invalid));
                };
                conds.push(predicate);
                for inner in cfg_attr.metas() {
                    self.meta(inner, conds, flags, count, item_path)?;
                }
                conds.pop();
            }
            view::Meta::UnsafeMeta(wrapped) => {
                if let Some(inner) = wrapped.meta() {
                    self.meta(inner, conds, flags, count, item_path)?;
                }
            }
            view::Meta::KeyValueMeta(key_value) => {
                let Some(path) = key_value.path() else { return Ok(()) };
                let (first, whole) = path_head(path);
                if whole && first == "path" {
                    let value = key_value
                        .syntax()
                        .descendants_with_tokens()
                        .filter_map(|element| element.into_token())
                        .find(|token| token.kind() == SyntaxKind::STRING)
                        .and_then(view::String::cast)
                        .and_then(|string| string.value().ok());
                    let Some(value) = value else {
                        return Err(self.cfg_error(item_path, cfgs::CfgRefused::Invalid));
                    };
                    let path = self.push_str(value.as_bytes());
                    let cond = self.conds(conds).map_err(|refused| self.cfg_error(item_path, refused))?;
                    self.paths.push(PathRow { path, cond });
                } else {
                    *flags |= attr_flags(first, whole);
                }
            }
            view::Meta::PathMeta(path_meta) => {
                if let Some(path) = path_meta.path() {
                    let (first, whole) = path_head(path);
                    *flags |= attr_flags(first, whole);
                }
            }
            view::Meta::TokenTreeMeta(tree) => {
                if let Some(path) = tree.path() {
                    let (first, whole) = path_head(path);
                    *flags |= attr_flags(first, whole);
                }
            }
        }
        Ok(())
    }

    /// The conjunction of `conds` into the cfg pool; empty for none.
    fn conds(&mut self, conds: &[view::CfgPredicate<'t>]) -> Result<Span, cfgs::CfgRefused> {
        if conds.is_empty() {
            return Ok(Span::EMPTY);
        }
        let at = self.cfgs.len() as u32;
        if conds.len() > 1 {
            cfgs::all_head(u16::try_from(conds.len()).map_err(|_| cfgs::CfgRefused::TooLarge)?, &mut self.cfgs);
        }
        for cond in conds {
            cfgs::encode(&CfgExpr::parse_from_view(*cond), &mut self.cfgs)?;
        }
        Ok(Span { at, len: self.cfgs.len() as u32 - at })
    }

    /// Gives each repeated (path, kind) a `#n` suffix in file order, so every row's stable id is
    /// its own.
    fn dedup_paths(&mut self) {
        let mut order: Vec<u32, A> = Vec::with_capacity_in(self.rows.len(), self.frame.clone());
        order.extend(0..self.rows.len() as u32);
        let strs = &self.strs;
        let rows = &self.rows;
        order.sort_by(|a, b| {
            let (a_row, b_row) = (&rows[*a as usize], &rows[*b as usize]);
            strs[a_row.path.range()]
                .cmp(&strs[b_row.path.range()])
                .then(a_row.kind.cmp(&b_row.kind))
                .then(a.cmp(b))
        });
        let mut run = 0u32;
        for at in 0..order.len() {
            let row = order[at] as usize;
            let same = at > 0 && {
                let previous = &self.rows[order[at - 1] as usize];
                previous.kind == self.rows[row].kind
                    && self.strs[previous.path.range()] == self.strs[self.rows[row].path.range()]
            };
            run = match same {
                true => run + 1,
                false => 0,
            };
            if run > 0 {
                let old = self.rows[row].path;
                let start = self.strs.len() as u32;
                self.strs.extend_from_within(old.range());
                self.strs.push(b'#');
                push_decimal(&mut self.strs, run);
                self.rows[row].path = Span { at: start, len: self.strs.len() as u32 - start };
            }
        }
    }

    /// The record: every table, the tables' root, the sections, the head.
    fn write(&self, key: &Key<FileImage>, lines: &[u32], tree: &[u8], file_cfg: Span, reading: u16) -> Vec<u8> {
        let mut out = Writer::new(Sections::SCHEMA);
        let tokens_at = out.push_slice(&self.tokens);
        let lines_at = out.push_slice(lines);
        let items_at = out.push_slice(&self.rows);
        let escapes_at = out.push_slice(&self.escapes);
        let strs_at = out.push_slice(&self.strs);
        let cfgs_at = out.push_slice(&self.cfgs);
        let lates_at = out.push_slice(&self.lates);
        let mods_at = out.push_slice(&self.mods);
        let paths_at = out.push_slice(&self.paths);
        // The parse record at a multiple of 16, as `syntax::image::read` asks of its bytes.
        out.push_slice::<u128>(&[]);
        let tree_at = out.push_slice(tree);
        let tables_at = out.next::<ImageTables>();
        let field = |offset: usize| tables_at + offset as u64;
        let tables = ImageTables {
            tokens: Slice::between(field(offset_of!(ImageTables, tokens)), tokens_at, self.tokens.len()),
            lines: Slice::between(field(offset_of!(ImageTables, lines)), lines_at, lines.len()),
            items: Slice::between(field(offset_of!(ImageTables, items)), items_at, self.rows.len()),
            escapes: Slice::between(field(offset_of!(ImageTables, escapes)), escapes_at, self.escapes.len()),
            strs: Slice::between(field(offset_of!(ImageTables, strs)), strs_at, self.strs.len()),
            cfgs: Slice::between(field(offset_of!(ImageTables, cfgs)), cfgs_at, self.cfgs.len()),
            lates: Slice::between(field(offset_of!(ImageTables, lates)), lates_at, self.lates.len()),
            mods: Slice::between(field(offset_of!(ImageTables, mods)), mods_at, self.mods.len()),
            paths: Slice::between(field(offset_of!(ImageTables, paths)), paths_at, self.paths.len()),
            tree: Slice::between(field(offset_of!(ImageTables, tree)), tree_at, tree.len()),
            file_cfg,
            reading: u32::from(reading),
            layout: LAYOUT,
        };
        let pushed = out.push(&tables);
        debug_assert_eq!(pushed, tables_at);
        let sections = [
            section_of::<ImageTables>(section::TABLES, tables_at, 1),
            section_of::<Tok>(section::TOKENS, tokens_at, self.tokens.len()),
            section_of::<u32>(section::LINES, lines_at, lines.len()),
            section_of::<ItemRow>(section::ITEMS, items_at, self.rows.len()),
            section_of::<ContentId>(section::ESCAPES, escapes_at, self.escapes.len()),
            section_of::<u8>(section::STRS, strs_at, self.strs.len()),
            section_of::<u8>(section::CFGS, cfgs_at, self.cfgs.len()),
            section_of::<LateRow>(section::LATES, lates_at, self.lates.len()),
            section_of::<ModRow>(section::MODS, mods_at, self.mods.len()),
            section_of::<PathRow>(section::PATHS, paths_at, self.paths.len()),
            section_of::<u8>(section::TREE, tree_at, tree.len()),
        ];
        let sections_at = out.push_slice(&sections);
        write_root::<FileImage>(out, key, sections_at, sections.len())
    }
}

/// Pushes the items of `body` that escape it ([`escapes`]) to `out`, outermost first: an item
/// that escapes is taken whole (what escapes its own bodies is found when its header is cut).
fn find_escapes<'t, A: Allocator>(body: view::Node<'t>, out: &mut Vec<view::Node<'t>, A>) {
    let mut walk = body.preorder();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if node != body && item_kind(node.kind()).is_some() && escapes(node) {
            out.push(node);
            walk.skip_subtree();
        }
    }
}

/// The section of `count` elements of `T` at `at`.
fn section_of<T>(id: u32, at: u64, count: usize) -> Section {
    Section { at, len: (count * size_of::<T>()) as u64, id, elem: size_of::<T>() as u32 }
}

/// Writes a `Sections` root of kind `K` over the sections at `sections_at` and finishes the
/// buffer.
fn write_root<K: Kind>(mut out: Writer, key: &Key<K>, sections_at: u64, count: usize) -> Vec<u8> {
    let root_at = out.next::<Sections>();
    let root = Sections {
        head: RecordHead { key: key.id(), tag: K::TAG.0, version: K::VERSION, flags: 0 },
        sections: Slice::between(root_at + offset_of!(Sections, sections) as u64, sections_at, count),
    };
    let pushed = out.push(&root);
    debug_assert_eq!(pushed, root_at);
    out.finish(pushed)
}

fn push_decimal<A: Allocator>(out: &mut Vec<u8, A>, value: u32) {
    let mut digits = [0u8; 10];
    let mut len = 0;
    let mut rest = value;
    loop {
        digits[len] = b'0' + (rest % 10) as u8;
        len += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    digits[..len].reverse();
    out.extend_from_slice(&digits[..len]);
}

/// `cfg(predicate)` under the `cfg_attr` conditions `conds`: `any(not(c1), any(not(c2), p))`.
fn implied<A: Allocator>(
    conds: &[view::CfgPredicate<'_>],
    predicate: view::CfgPredicate<'_>,
    out: &mut Vec<u8, A>,
) -> Result<(), cfgs::CfgRefused> {
    for cond in conds {
        cfgs::implies_head(out);
        cfgs::encode(&CfgExpr::parse_from_view(*cond), out)?;
    }
    cfgs::encode(&CfgExpr::parse_from_view(predicate), out)
}

/// An attribute's flags by its path.
fn attr_flags(first: &str, whole: bool) -> u32 {
    let mut flags = 0;
    if whole {
        flags |= match first {
            "macro_export" => A_MACRO_EXPORT,
            "no_mangle" | "export_name" | "used" | "link_section" => A_EXPORT,
            "link" => A_LINK,
            "derive" => A_DERIVE,
            _ => 0,
        };
    }
    if !quiet::is_inert(first, whole) {
        flags |= A_MACRO;
    }
    flags
}

/// The flags of `item`'s own attributes, without their `#[cfg]`s (what the escape rules and
/// the cut ask; an attribute under a `cfg_attr` counts as present, which can only keep more
/// text in a digest).
fn own_flags(item: view::Node<'_>) -> u32 {
    let mut flags = 0;
    for child in item.children() {
        if child.kind() != SyntaxKind::ATTR {
            continue;
        }
        let Some(attr) = view::Attr::cast(child) else { continue };
        // `Meta::path` sees through `unsafe(..)`; a `cfg` has none.
        for meta in attr.skip_cfg_attrs() {
            if let Some(path) = meta.path() {
                let (first, whole) = path_head(path);
                flags |= attr_flags(first, whole);
            }
        }
    }
    flags
}

/// Whether `item`, written in a body, escapes it: an impl, a `#[macro_export]` macro, an item
/// with `#[no_mangle]`, `#[export_name]`, `#[used]` or `#[link_section]`, a `#[link]` block.
/// Its header digest joins the enclosing item's piece, so editing it moves the surface key.
pub fn escapes(item: view::Node<'_>) -> bool {
    if item.kind() == SyntaxKind::IMPL {
        return true;
    }
    let flags = own_flags(item);
    flags & A_EXPORT != 0
        || (item.kind() == SyntaxKind::MACRO_RULES && flags & A_MACRO_EXPORT != 0)
        || (item.kind() == SyntaxKind::EXTERN_BLOCK && flags & A_LINK != 0)
}

const _: () = assert!(rows::NONE == u32::MAX);
