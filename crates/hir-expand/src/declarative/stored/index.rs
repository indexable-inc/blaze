//! A file's index: for every item written in it, what the item is made of, as three hashes
//! (AGENTS.md rule 65; docs/v2/plan/design/restore.md, "Items read in place").
//!
//! One entry a file, under the file's content and how it is read (its edition and syntax
//! extensions; [`super::file_set`]): never under a crate. A file whose content the store has
//! seen is indexed with no parse; one whose content is new is indexed from its one parse.
//! Every per-item entry (an item's attribute flags, an impl's assoc items, a body) is keyed
//! by parts of its item's [`ItemSource`], so an edit of one item of a file moves the file's
//! index and the keys of that item's entries alone.
//!
//! A row an item, by its ast id: each item of the file and of its inline modules, each assoc
//! item of an `impl` or a `trait`, each item of an `extern` block, each enum variant, and the
//! file itself (its inner attributes). An item in a body has no row: it is keyed under the
//! body it is in.
//!
//! After the rows, a pair an assoc item: the ast id of its `impl` or `trait`, then its own,
//! in the order written ([`FileIndex::assoc_items`]). An ast id is of its file (its hash is of
//! its parent's id and its index counts the file's other items of that hash), so it is kept
//! here, under the file's content, and in no entry keyed by an item's text: such an entry
//! names an assoc item by where it is written in its `impl`, and its reader asks this which
//! id that is in its own file.
//!
//! The hashes are of tokens ([`tokens`]): each token's kind and text, lexed with no parse, with
//! whitespace and plain comments left out, so an item moved, reindented or commented the same
//! is the same row, and nothing a parse decides (a lazy node or a parsed one, where the item is
//! in its file) is in them: an outline and a full parse of one text make the same bytes. What
//! is left out is what nothing filed under a row can see: the parser and a token tree see a
//! token's spacing only as whether a punctuation mark is glued to the next ([`glues`]), which
//! is fed; whatever reads more of the text (`stringify!`'s and `assert!`'s spacing, a line, a
//! column, a proc macro's `Span::source_text`) reads the file's text, a read no key of an item
//! covers (`blaze_item_hash::reads` refuses it by name; [`super::expansion`] keys no such
//! builtin), and a location macro expands to a placeholder whose value no record holds
//! (`hir_expand::builtin::PLACE_PLACEHOLDER`; `hir_ty::body_store::mir::location_literals`).
//!
//! Last, the positions: a segment of its own, a range a row in the rows' order, where each
//! item is written in the file ([`FileIndex::range`]). A position is of the file, as the
//! index is (its key is the file's content), and in no row and no entry keyed by an item's
//! text: a reader that needs where an item is (whether a mark of the file's text is inside
//! it, its line) reads it here, with no parse and no ast id map, and an entry of the item is
//! not moved by an edit that only moves it.
//!
//! The invariant: equal keys are equal bytes; a row is a function of its item's tokens alone,
//! and a position of the file's text.

use base_db::{Crate, Db, EditionedFileId};
use blaze_ids::{ContentId, Hasher};
use span::{AstIdMap, ErasedFileAstId, ROOT_ERASED_FILE_AST_ID, TextRange, TextSize};
use syntax::{
    Edition, LexedStr, NodeOrToken, SyntaxKind, ast,
    view::{self, AstNode as _, HasModuleItem as _},
};

use crate::{HirFileId, MacroCallId};

type R<T = ()> = Result<T, String>;

/// The layout of a file's index: part of its key and its second word. It changes with the
/// header, a row, which nodes have rows, what any of the three hashes covers, or the pairs.
/// 2: the pairs of assoc items after the rows, and their count in the header. 3: the
/// positions after the pairs, a range a row. 4: the three hashes are of tokens ([`tokens`]),
/// not of text: whitespace and plain comments are in none of them.
pub const LAYOUT: u32 = 4;

/// The family of a file's index, as the entry store names it.
pub const FAMILY: &str = "file index";

const INDEX_DOMAIN: &str = "blaze v2 file index";
// Each " 2" since layout 4, the hashes of tokens: no hash of tokens is one of text, so no
// entry kept under a hash of text is found by one of tokens.
const ATTRS_DOMAIN: &str = "blaze v2 item attrs 2";
const HEADER_DOMAIN: &str = "blaze v2 item header 2";
const BODY_DOMAIN: &str = "blaze v2 item body 2";
const WHOLE_DOMAIN: &str = "blaze v2 item tokens 1";

/// The edition every text is lexed in for its hashes, whatever the edition of its file: a
/// hash is a function of the text alone. A later edition only reserves more (a C string, a raw
/// lifetime, a guarded string are one token there and several before), never splitting what an
/// earlier one joins, so two texts the same token for token in this edition are the same token
/// for token in theirs; where this one tells two apart that their own does not, the two hash
/// apart where they need not, never alike where they must not. A lexer error is fed with its
/// token ([`Token::feed`]): a prefix this edition reserves is told from the same text spaced.
const LEX_EDITION: Edition = Edition::LATEST;

/// What an index starts with.
const MAGIC: [u8; 8] = *b"blzfidx\0";
/// Bytes before the rows: the magic, the layout, the count of rows, the count of pairs.
const HEAD: usize = 20;
/// Bytes of a row: the ast id, whether it has a body, and the three hashes.
const ROW: usize = 56;
/// Bytes of a pair: the ast id of an `impl` or a `trait`, then of one assoc item written in it.
const PAIR: usize = 8;
/// Bytes of a position: where a row's item starts in the file and where it ends.
const SPAN: usize = 8;

/// In a header's feed, where a body was cut. A mark's first two bytes are `0xffff` or
/// `0xfffe`, which no token's kind word is (every kind is below 1024), so a mark is told from a
/// piece's next token and from its end ([`Feed::piece`]).
const CUT: u32 = u32::MAX;
/// Ends the inner attributes after a cut, and the outer attributes of an item.
const END: u32 = u32::MAX - 1;

/// What one item is made of. The one definition of an item's source hashes: every lane's
/// per-item key takes its parts from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ItemSource {
    /// The item's attributes and doc comments, outer then inner, wherever the item is.
    pub attrs: u128,
    /// The item with every body cut (its own, its assoc items', its variants' discriminants,
    /// an inline module's items), the inner attributes of each cut body kept. Covers `attrs`.
    pub header: u128,
    /// The item's own body node (a `fn`'s block, a `const`'s or `static`'s value, a variant's
    /// discriminant), and nothing of its header; `None` for an item with none.
    pub body: Option<u128>,
    /// Whether a token `attrs` covers has `cfg` in it: only then can what is made of the
    /// attributes differ with the crate's cfg. Of tokens, as the hash is: a plain comment
    /// that names `cfg` sets it in no row.
    pub attrs_cfg: bool,
    /// The same of the tokens `header` covers.
    pub header_cfg: bool,
    /// Of an `impl` or a `trait`: its assoc items are a function of its header and the cfg
    /// alone, with no name looked up. No macro call at assoc position, no inner attribute in
    /// an assoc `fn`'s body, and every attribute of an assoc item one word that is built in
    /// (as `DefMap::is_builtin_or_registered_attr` says of any crate) and not `cfg_attr`.
    pub plain: bool,
}

const HAS_BODY: u32 = 1;
const ATTRS_CFG: u32 = 2;
const HEADER_CFG: u32 = 4;
const PLAIN: u32 = 8;
/// Every bit of a row's flag word.
const FLAGS: u32 = 15;

/// The index of one file, as its entry's bytes, read in place.
#[derive(Debug, PartialEq, Eq)]
pub struct FileIndex {
    bytes: Box<[u8]>,
}

impl FileIndex {
    /// How many rows there are.
    pub fn len(&self) -> usize {
        u32_at(&self.bytes, 12) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entry's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn row(&self, at: usize) -> (u32, ItemSource) {
        let row = &self.bytes[HEAD + at * ROW..HEAD + (at + 1) * ROW];
        let flags = u32_at(row, 4);
        let source = ItemSource {
            attrs: u128_at(row, 8),
            header: u128_at(row, 24),
            body: (flags & HAS_BODY != 0).then(|| u128_at(row, 40)),
            attrs_cfg: flags & ATTRS_CFG != 0,
            header_cfg: flags & HEADER_CFG != 0,
            plain: flags & PLAIN != 0,
        };
        (u32_at(row, 0), source)
    }

    /// How many assoc items are listed.
    fn pairs(&self) -> usize {
        u32_at(&self.bytes, 16) as usize
    }

    /// Where the row of the item whose ast id is `ast` is: a binary search of the rows.
    fn find(&self, ast: ErasedFileAstId) -> Option<usize> {
        let want = ast.into_raw();
        let (mut low, mut high) = (0, self.len());
        // The bound: the range halves each turn.
        while low < high {
            let mid = low + (high - low) / 2;
            match u32_at(&self.bytes, HEAD + mid * ROW).cmp(&want) {
                std::cmp::Ordering::Equal => return Some(mid),
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
            }
        }
        None
    }

    /// The row of the item whose ast id is `ast`: a binary search of the rows.
    pub fn get(&self, ast: ErasedFileAstId) -> Option<ItemSource> {
        self.find(ast).map(|at| self.row(at).1)
    }

    /// Where the item whose ast id is `ast` is written in the file, as its node's range: the
    /// positions segment, read with no parse. `None` for a node with no row (an item in a
    /// body, which its body's syntax places).
    pub fn range(&self, ast: ErasedFileAstId) -> Option<TextRange> {
        let at = self.find(ast)?;
        let base = HEAD + self.len() * ROW + self.pairs() * PAIR + at * SPAN;
        let (start, end) = (u32_at(&self.bytes, base), u32_at(&self.bytes, base + 4));
        Some(TextRange::new(TextSize::from(start), TextSize::from(end)))
    }

    /// Every row, by ast id.
    pub fn rows(&self) -> impl Iterator<Item = (ErasedFileAstId, ItemSource)> + '_ {
        (0..self.len()).map(|at| {
            let (id, source) = self.row(at);
            (ErasedFileAstId::from_raw(id), source)
        })
    }

    /// The assoc items written in the `impl` or `trait` whose ast id is `owner`, in the order
    /// written, a macro call at assoc position among them: none for any other id. A binary
    /// search of the pairs.
    pub fn assoc_items(&self, owner: ErasedFileAstId) -> AssocItems<'_> {
        let want = owner.into_raw();
        let count = self.pairs();
        let start = HEAD + self.len() * ROW;
        let pairs = &self.bytes[start..start + count * PAIR];
        // The first pair whose owner is not below `want`. The bound: the range halves each turn.
        let (mut low, mut high) = (0, count);
        while low < high {
            let mid = low + (high - low) / 2;
            if u32_at(pairs, mid * PAIR) < want {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        // The bound: the pairs of one owner, which are together.
        let mut end = low;
        while end < count && u32_at(pairs, end * PAIR) == want {
            end += 1;
        }
        AssocItems { pairs: &pairs[low * PAIR..end * PAIR] }
    }
}

/// The assoc items written in one `impl` or `trait`, in the order written
/// ([`FileIndex::assoc_items`]): the ast id each has in the index's file.
#[derive(Clone, Copy, Debug)]
pub struct AssocItems<'a> {
    pairs: &'a [u8],
}

impl AssocItems<'_> {
    /// How many are written, whatever a cfg leaves of them.
    pub fn len(&self) -> usize {
        self.pairs.len() / PAIR
    }

    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The ast id of the `at`th written, from 0.
    pub fn get(&self, at: usize) -> Option<ErasedFileAstId> {
        (at < self.len()).then(|| ErasedFileAstId::from_raw(u32_at(self.pairs, at * PAIR + 4)))
    }

    /// Which one written has the ast id `id`, from 0: a scan of one `impl`'s items.
    pub fn position(&self, id: ErasedFileAstId) -> Option<usize> {
        let want = id.into_raw();
        (0..self.len()).find(|&at| u32_at(self.pairs, at * PAIR + 4) == want)
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
}

fn u128_at(bytes: &[u8], at: usize) -> u128 {
    u128::from_le_bytes(bytes[at..at + 16].try_into().expect("sixteen bytes"))
}

/// The key of the index of the file whose set is `set` ([`super::file_set`]).
pub fn key(set: &ContentId) -> ContentId {
    let mut hasher = Hasher::new(INDEX_DOMAIN);
    hasher.id(set).update(&LAYOUT.to_le_bytes());
    hasher.finish()
}

/// The index of `file`, read for `krate` (named by the error of an index that does not
/// read). Found by the file's content where this database keeps entries, with no parse; made
/// from the file's parse and kept where it is not found. A database that keeps no entries
/// makes it from the parse.
#[blaze_query::tracked(returns(ref), mem_tag = Expand)]
pub fn of(db: &Db, file: EditionedFileId, krate: Crate) -> FileIndex {
    let kept = super::of(db);
    let set = super::file_set(db, file);
    let key = key(&set);
    if let Some(kept) = kept {
        let mut found = None;
        kept.get(db, krate, FAMILY, &set, key, &mut |bytes| {
            found = Some(decode(bytes)?);
            Ok(())
        });
        if let Some(found) = found {
            return found;
        }
    }
    let bytes = make(file.parse(db).root(), HirFileId::FileId(file).ast_id_map(db));
    if let Some(kept) = kept {
        kept.put(db, krate, FAMILY, &set, key, &bytes);
    }
    FileIndex { bytes: bytes.into_boxed_slice() }
}

/// The index of `call`'s expansion, read for `krate`, where the expansion has a key
/// ([`super::expansion::key`]: its inputs, never its text): found by that key where this
/// database keeps entries, with no expansion; made from the expansion's parse and kept where
/// it is not found. `None` for an expansion with no key, whose items are made from it.
///
/// A row is a function of its item's tokens, as a file's is; the positions are of the
/// expansion's text, which the key determines (an expansion's text is made from its tokens
/// and their spacing, never from the spaces of the text its call or definition is in).
#[blaze_query::tracked(returns(ref), mem_tag = Expand)]
pub fn of_expansion(db: &Db, call: MacroCallId, krate: Crate) -> Option<FileIndex> {
    let set = super::expansion::key(db, call)?;
    let key = key(&set);
    let kept = super::of(db);
    if let Some(kept) = kept {
        let mut found = None;
        kept.get(db, krate, FAMILY, &set, key, &mut |bytes| {
            found = Some(decode(bytes)?);
            Ok(())
        });
        if let Some(found) = found {
            return Some(found);
        }
    }
    let file = HirFileId::MacroFile(call);
    let bytes = make(file.root(db), file.ast_id_map(db));
    if let Some(kept) = kept {
        kept.put(db, krate, FAMILY, &set, key, &bytes);
    }
    Some(FileIndex { bytes: bytes.into_boxed_slice() })
}

/// The index of `file`, a file's ([`of`]) or an expansion's ([`of_expansion`]): `None` for an
/// expansion with no key.
pub fn of_any(db: &Db, file: HirFileId, krate: Crate) -> Option<&FileIndex> {
    match file {
        HirFileId::FileId(file) => Some(of(db, file, krate)),
        HirFileId::MacroFile(call) => of_expansion(db, call, krate).as_ref(),
    }
}

/// The source hashes of the item `ast` of `file`: `None` for an item of an expansion with no
/// key ([`super::expansion::key`]) and for a node with no row.
pub fn source(db: &Db, file: HirFileId, ast: ErasedFileAstId, krate: Crate) -> Option<ItemSource> {
    of_any(db, file, krate)?.get(ast)
}

/// Where the item `ast` of `file` is written ([`FileIndex::range`]): `None` for an item of an
/// expansion with no key and for a node with no row. No parse or expansion where the index is
/// kept.
pub fn range(db: &Db, file: HirFileId, ast: ErasedFileAstId, krate: Crate) -> Option<TextRange> {
    of_any(db, file, krate)?.range(ast)
}

/// The hash of every token of the item `ast` of `file`, nothing cut ([`whole_tokens`]): what a
/// derive or an attribute macro on the item is given, every body included (an enum's
/// discriminants, an `impl`'s methods), which its row's `header` and `body` leave out. Read
/// from where the index places the item and the file's text, with no parse. `None` for a node
/// with no row.
pub fn whole(db: &Db, file: EditionedFileId, ast: ErasedFileAstId, krate: Crate) -> Option<u128> {
    let range = of(db, file, krate).range(ast)?;
    let text: &str = db.file_text(file.file_id(db)).text(db);
    Some(whole_tokens(text.get(std::ops::Range::<usize>::from(range))?))
}

/// The hash of every token of `text` ([`tokens`]), one piece.
pub fn whole_tokens(text: &str) -> u128 {
    let mut feed = Feed::new(WHOLE_DOMAIN);
    feed.piece(text);
    feed.finish().0
}

/// The index of the file whose tree is `root` and whose ast ids are `map`, as its entry's
/// bytes.
pub fn make(root: view::Node<'_>, map: &AstIdMap) -> Vec<u8> {
    // alloc: one file's pairs, while its index is made
    let mut assoc = Vec::new();
    let mut rows = placed_rows(root, map, &mut assoc);
    rows.sort_unstable_by_key(|&(id, _, _)| id);
    if let Some(pair) = rows.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        panic!("file index: two items of one file with ast id {:#x}", pair[0].0);
    }
    // By owner, the order written kept within one: the sort is stable.
    // alloc: the sort's buffer, once per index made
    assoc.sort_by_key(|pair| pair[0]);
    // alloc: one file's index, kept as the query's value
    let mut out =
        Vec::with_capacity(HEAD + rows.len() * (ROW + SPAN) + assoc.len() * PAIR);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&LAYOUT.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&(assoc.len() as u32).to_le_bytes());
    for &(id, source, _) in &rows {
        out.extend_from_slice(&id.to_le_bytes());
        let flags = if source.body.is_some() { HAS_BODY } else { 0 }
            | if source.attrs_cfg { ATTRS_CFG } else { 0 }
            | if source.header_cfg { HEADER_CFG } else { 0 }
            | if source.plain { PLAIN } else { 0 };
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&source.attrs.to_le_bytes());
        out.extend_from_slice(&source.header.to_le_bytes());
        out.extend_from_slice(&source.body.unwrap_or(0).to_le_bytes());
    }
    for [owner, item] in assoc {
        out.extend_from_slice(&owner.to_le_bytes());
        out.extend_from_slice(&item.to_le_bytes());
    }
    for &(_, _, range) in &rows {
        out.extend_from_slice(&u32::from(range.start()).to_le_bytes());
        out.extend_from_slice(&u32::from(range.end()).to_le_bytes());
    }
    out
}

/// An index from its entry's bytes: `Err` saying what is wrong with bytes this did not write,
/// a previous layout by its number.
pub fn decode(bytes: &[u8]) -> R<FileIndex> {
    if bytes.len() < HEAD || bytes[..8] != MAGIC {
        // alloc: error path
        return Err(format!("{} bytes that do not start as a file index", bytes.len()));
    }
    let layout = u32_at(bytes, 8);
    if layout != LAYOUT {
        // alloc: error path
        return Err(format!("file index layout {layout}, and this reads layout {LAYOUT}"));
    }
    let (count, pairs) = (u32_at(bytes, 12) as usize, u32_at(bytes, 16) as usize);
    let size =
        HEAD as u64 + count as u64 * (ROW + SPAN) as u64 + pairs as u64 * PAIR as u64;
    if bytes.len() as u64 != size {
        // alloc: error path
        return Err(format!(
            "a file index of {count} rows in {} bytes, with {pairs} assoc items listed",
            bytes.len()
        ));
    }
    // alloc: the query's value, one file's index
    let index = FileIndex { bytes: bytes.into() };
    let mut last = None;
    for at in 0..count {
        let (id, _) = index.row(at);
        let flag = u32_at(&index.bytes[HEAD + at * ROW..], 4);
        if last.is_some_and(|last| last >= id) || flag & !FLAGS != 0 {
            // alloc: error path
            return Err(format!("a file index whose row {at} (ast id {id:#x}) is out of order"));
        }
        last = Some(id);
    }
    let listed = &index.bytes[HEAD + count * ROW..];
    let mut last = 0;
    for at in 0..pairs {
        let owner = u32_at(listed, at * PAIR);
        if owner < last {
            // alloc: error path
            return Err(format!(
                "a file index whose assoc pair {at} (of {owner:#x}) is out of order"
            ));
        }
        last = owner;
    }
    let placed = &index.bytes[HEAD + count * ROW + pairs * PAIR..];
    for at in 0..count {
        let (start, end) = (u32_at(placed, at * SPAN), u32_at(placed, at * SPAN + 4));
        if start > end {
            // alloc: error path
            return Err(format!(
                "a file index whose position {at} ends at {end}, before its start {start}"
            ));
        }
    }
    Ok(index)
}

/// Every row of the file, in the file's order: the file itself first. Into `assoc`, a pair
/// an assoc item (its `impl` or `trait`, then itself), in the file's order.
#[cfg(test)]
fn rows(
    root: view::Node<'_>,
    map: &AstIdMap,
    assoc: &mut Vec<[u32; 2]>,
) -> Vec<(u32, ItemSource)> {
    placed_rows(root, map, assoc).into_iter().map(|(id, source, _)| (id, source)).collect()
}

/// A row as it is made: the ast id, the source hashes, and where the node is in its file.
type Row = (u32, ItemSource, TextRange);

/// [`rows`], each with where its node is written.
fn placed_rows(root: view::Node<'_>, map: &AstIdMap, assoc: &mut Vec<[u32; 2]>) -> Vec<Row> {
    // alloc: one file's rows, while its index is made
    let mut out = Vec::new();
    let mut attrs = Feed::new(ATTRS_DOMAIN);
    attrs.mark(END);
    inner_attrs(root, &mut attrs);
    let (attrs, cfg) = attrs.finish();
    // The file has no header of its own: its items have their rows.
    let file = ItemSource {
        attrs,
        header: attrs,
        body: None,
        attrs_cfg: cfg,
        header_cfg: cfg,
        plain: false,
    };
    out.push((ROOT_ERASED_FILE_AST_ID.into_raw(), file, root.text_range()));
    if let Some(file) = view::SourceFile::cast(root) {
        for item in file.items() {
            item_rows(item, map, &mut out, assoc);
        }
    } else if let Some(expanded) = view::MacroItems::cast(root) {
        // An expansion at item or assoc position ([`of_expansion`]): its items as a file's.
        for item in expanded.items() {
            item_rows(item, map, &mut out, assoc);
        }
    }
    out
}

fn item_rows(
    item: view::Item<'_>,
    map: &AstIdMap,
    out: &mut Vec<Row>,
    assoc: &mut Vec<[u32; 2]>,
) {
    let id = map.ast_id_of_view::<ast::Item>(item).erase().into_raw();
    out.push((id, item_source(item), item.syntax().text_range()));
    match item {
        view::Item::Module(it) => {
            for child in it.item_list().into_iter().flat_map(|list| list.items()) {
                item_rows(child, map, out, assoc);
            }
        }
        view::Item::ExternBlock(it) => {
            for child in it.extern_item_list().into_iter().flat_map(|list| list.extern_items()) {
                let child = match child {
                    view::ExternItem::Fn(it) => view::Item::Fn(it),
                    view::ExternItem::MacroCall(it) => view::Item::MacroCall(it),
                    view::ExternItem::Static(it) => view::Item::Static(it),
                    view::ExternItem::TypeAlias(it) => view::Item::TypeAlias(it),
                };
                item_rows(child, map, out, assoc);
            }
        }
        view::Item::Impl(it) => assoc_rows(id, it.assoc_item_list(), map, out, assoc),
        view::Item::Trait(it) => assoc_rows(id, it.assoc_item_list(), map, out, assoc),
        view::Item::Enum(it) => {
            for variant in it.variant_list().into_iter().flat_map(|list| list.variants()) {
                let id = map.ast_id_of_view::<ast::Variant>(variant).erase().into_raw();
                let body = variant.const_arg().map(|it| it.syntax());
                let source = node_source(variant.syntax(), body.as_slice(), body, None, false);
                out.push((id, source, variant.syntax().text_range()));
            }
        }
        _ => {}
    }
}

/// The rows of the assoc items of `list`, and the pair of each with `owner`, the `impl` or
/// `trait` the list is of.
fn assoc_rows(
    owner: u32,
    list: Option<view::AssocItemList<'_>>,
    map: &AstIdMap,
    out: &mut Vec<Row>,
    assoc: &mut Vec<[u32; 2]>,
) {
    for child in list.into_iter().flat_map(|list| list.assoc_items()) {
        let child = assoc_as_item(child);
        assoc.push([owner, map.ast_id_of_view::<ast::Item>(child).erase().into_raw()]);
        item_rows(child, map, out, assoc);
    }
}

fn assoc_as_item(item: view::AssocItem<'_>) -> view::Item<'_> {
    match item {
        view::AssocItem::Const(it) => view::Item::Const(it),
        view::AssocItem::Fn(it) => view::Item::Fn(it),
        view::AssocItem::MacroCall(it) => view::Item::MacroCall(it),
        view::AssocItem::TypeAlias(it) => view::Item::TypeAlias(it),
    }
}

/// The body of an item that has one of its own.
fn body_of(item: view::Item<'_>) -> Option<view::Node<'_>> {
    match item {
        view::Item::Fn(it) => it.body().map(|it| it.syntax()),
        view::Item::Const(it) => it.body().map(|it| it.syntax()),
        view::Item::Static(it) => it.body().map(|it| it.syntax()),
        _ => None,
    }
}

/// The source hashes of `item`, from its syntax alone: what its row in its file's index
/// holds, for an item whose file has none (one a macro wrote).
pub fn item_source(item: view::Item<'_>) -> ItemSource {
    let body = body_of(item);
    // alloc: the few cuts of one item, while it is hashed
    let mut cuts = Vec::new();
    cuts.extend(body);
    let mut inner = body.filter(|_| matches!(item, view::Item::Fn(_)));
    // A function, not a closure: the list and the cuts share one lifetime, which a closure's
    // elided lifetimes cannot say.
    fn assoc<'a>(list: Option<view::AssocItemList<'a>>, cuts: &mut Vec<view::Node<'a>>) {
        for child in list.into_iter().flat_map(|list| list.assoc_items()) {
            cuts.extend(body_of(assoc_as_item(child)));
        }
    }
    let mut plain = false;
    match item {
        view::Item::Impl(it) => {
            inner = it.assoc_item_list().map(|it| it.syntax());
            assoc(it.assoc_item_list(), &mut cuts);
            plain = is_plain(it.assoc_item_list());
        }
        view::Item::Trait(it) => {
            inner = it.assoc_item_list().map(|it| it.syntax());
            assoc(it.assoc_item_list(), &mut cuts);
            plain = is_plain(it.assoc_item_list());
        }
        view::Item::Enum(it) => {
            for variant in it.variant_list().into_iter().flat_map(|list| list.variants()) {
                cuts.extend(variant.const_arg().map(|it| it.syntax()));
            }
        }
        view::Item::Module(it) => {
            inner = it.item_list().map(|it| it.syntax());
            cuts.extend(inner);
        }
        view::Item::ExternBlock(it) => inner = it.extern_item_list().map(|it| it.syntax()),
        _ => {}
    }
    node_source(item.syntax(), &cuts, body, inner, plain)
}

/// Whether the assoc items of `list` are found with no name looked up ([`ItemSource::plain`]).
fn is_plain(list: Option<view::AssocItemList<'_>>) -> bool {
    list.into_iter().flat_map(|list| list.assoc_items()).all(|item| {
        let body = match item {
            view::AssocItem::MacroCall(_) => return false,
            view::AssocItem::Fn(it) => it.body().map(|it| it.syntax()),
            view::AssocItem::Const(_) | view::AssocItem::TypeAlias(_) => None,
        };
        if body.is_some_and(|it| view::block_may_start_with_inner_attr(it.text(), true)) {
            return false;
        }
        item.syntax().children_with_tokens().all(|child| match child {
            NodeOrToken::Node(child) => view::Attr::cast(child).is_none_or(is_inert),
            NodeOrToken::Token(_) => true,
        })
    })
}

/// Whether `attr` is an attribute no crate resolves as a macro: `cfg(..)`, or one word, built
/// in or `rustc_`'s, and not `cfg_attr`, which may write any attribute.
///
/// `cfg(..)` and `cfg_attr(..)` are metas of their own kinds in the tree, a predicate and no
/// path (`Meta::path` is `None` of both), so they are told by kind: asked for its path, a
/// `#[cfg(..)]` was no attribute at all, and no `impl` with a cfg'd assoc item was plain.
fn is_inert(attr: view::Attr<'_>) -> bool {
    let path = match attr.meta() {
        Some(view::Meta::CfgMeta(_)) => return true,
        Some(view::Meta::CfgAttrMeta(_)) | None => return false,
        Some(meta) => meta.path(),
    };
    let Some(path) = path else { return false };
    let word = path.syntax().text();
    word != "cfg_attr" && syntax::builtin_attrs::is_built_in(word)
}

/// The hashes of `node`: its header with `cuts` (nodes within it, in its order, none within
/// another) left out, `body` as its body and `inner` as the node holding its inner
/// attributes.
fn node_source(
    node: view::Node<'_>,
    cuts: &[view::Node<'_>],
    body: Option<view::Node<'_>>,
    inner: Option<view::Node<'_>>,
    plain: bool,
) -> ItemSource {
    let mut attrs = Feed::new(ATTRS_DOMAIN);
    for child in node.children_with_tokens() {
        if let NodeOrToken::Node(child) = child
            && view::AnyAttr::can_cast(child.kind())
        {
            attrs.piece(child.text());
        }
    }
    attrs.mark(END);
    if let Some(inner) = inner {
        inner_attrs(inner, &mut attrs);
    }

    // The node is lexed once, its tokens dealt out as they come: the header's to the header,
    // each cut's left out of it but for the cut's mark (the body's to the body). Lexed whole,
    // a token at the edge of a cut is told whether it is glued to the next across the edge
    // (`= -1` against `=-1`), which pieces lexed apart could not say.
    let mut header = Feed::new(HEADER_DOMAIN);
    header.kind(node.kind());
    let mut own = body.map(|body| {
        let mut feed = Feed::new(BODY_DOMAIN);
        feed.kind(body.kind());
        (body.text_range(), feed)
    });
    let start = node.text_range().start();
    let mut cuts = cuts.iter().copied().peekable();
    // The range of the cut the walk is in.
    let mut within: Option<TextRange> = None;
    /// Ends the header's piece before `cut` and marks the cut, with its inner attributes.
    fn open(cut: view::Node<'_>, header: &mut Feed) {
        header.end();
        header.mark(CUT);
        header.kind(cut.kind());
        inner_attrs(cut, header);
        header.mark(END);
    }
    tokens(node.text(), |at, token| {
        let at = start + TextSize::from(at as u32);
        if let Some(range) = within {
            if at < range.end() {
                if let Some((body, feed)) = &mut own
                    && *body == range
                {
                    token.feed(feed);
                }
                return;
            }
            within = None;
        }
        while let Some(cut) = cuts.next_if(|cut| cut.text_range().start() <= at) {
            open(cut, &mut header);
            let range = cut.text_range();
            if at < range.end() {
                within = Some(range);
                if let Some((body, feed)) = &mut own
                    && *body == range
                {
                    token.feed(feed);
                }
                return;
            }
        }
        token.feed(&mut header);
    });
    // A cut with no token after the last (none is: a cut is a node, which holds one).
    for cut in cuts {
        open(cut, &mut header);
    }
    header.end();

    let body = own.map(|(_, mut feed)| {
        feed.end();
        feed.finish().0
    });
    let (attrs, attrs_cfg) = attrs.finish();
    let (header, header_cfg) = header.finish();
    ItemSource { attrs, header, body, attrs_cfg, header_cfg, plain }
}

/// One token of a text, as [`tokens`] gives it.
#[derive(Clone, Copy)]
struct Token<'a> {
    kind: SyntaxKind,
    text: &'a str,
    /// The lexer's error of it, when it has one.
    error: Option<&'a str>,
    /// Whether it is glued to the next token in a way the parser or a token tree sees
    /// ([`glues`]): `false` for any other token, glued or not.
    glued: bool,
}

impl Token<'_> {
    /// Feeds the token: its kind's word, a byte of whether it is glued and whether it has an
    /// error, its text's length and its text whole (a string's or a raw string's too, every
    /// space in it), then its error's length and text if it has one. Every part has its length
    /// or a fixed size, so two runs of tokens feed the same bytes only when they are the same
    /// tokens: `a b` (two identifiers) is not `ab` (one), and `- -` is not `--` (glued).
    fn feed(self, feed: &mut Feed) {
        feed.cfg |= self.text.contains("cfg");
        let flags = u8::from(self.glued) | u8::from(self.error.is_some()) << 1;
        feed.hasher
            .update(&(self.kind as u16).to_le_bytes())
            .update(&[flags])
            .update(&(self.text.len() as u32).to_le_bytes())
            .update(self.text.as_bytes());
        if let Some(error) = self.error {
            feed.hasher.update(&(error.len() as u32).to_le_bytes()).update(error.as_bytes());
        }
    }
}

/// Calls `each` with every token of `text` but whitespace and plain comments (`//`, `/* */`),
/// in order, with where it starts in `text`. Doc comments are tokens that stay: they are
/// attributes (`#[doc = ".."]`), which a proc macro, `cfg_attr` and rustdoc read, inner
/// (`//!`, `/*!`) as outer (`///`, `/**`). Lexed with no parse ([`LEX_EDITION`]), so a lazy
/// body stays unparsed; the lexer lends its columns from the worker's scratch and nothing is
/// allocated per token: what each token is goes straight to its caller.
///
/// The cost against the floor (one pass over the item's bytes to hash them): one more pass to
/// lex them, and the hash then reads each token's bytes once with a few bytes of kind and
/// length, which a run of whitespace and comments makes up for.
fn tokens(text: &str, mut each: impl FnMut(usize, Token<'_>)) {
    let lexed = LexedStr::new(LEX_EDITION, text);
    // The last token seen, held until the next says whether it is glued to it.
    let mut held: Option<(usize, Token<'_>)> = None;
    let mut gap = false;
    let mut end = 0;
    for at in 0..lexed.len() {
        let start = end;
        end += lexed.text_len(at);
        let kind = lexed.kind(at);
        if kind.is_trivia() {
            gap = true;
            continue;
        }
        let token = Token { kind, text: &text[start..end], error: lexed.error(at), glued: false };
        if let Some((held_at, mut last)) = held.take() {
            last.glued = !gap && glues(last, token);
            each(held_at, last);
        }
        held = Some((start, token));
        gap = false;
    }
    // The last token of the text is glued to nothing: a token tree made of the text ends there.
    if let Some((held_at, last)) = held {
        each(held_at, last);
    }
}

/// Calls `each` with where each token [`tokens`] gives of `text` starts and its length, in
/// order: the tokens a row hashes, so what a kept value says by them (`super::Squeeze`) is a
/// function of the row's hash, as its key is.
pub(super) fn token_ranges(text: &str, mut each: impl FnMut(u32, u32)) {
    tokens(text, |at, token| each(at as u32, token.text.len() as u32));
}

/// Whether whitespace or a comment between `left` and `right` is something the parser or a
/// token tree can see, so whether they are glued is fed: the parser joins `-` `>` into `->`,
/// `&` `&` into `&&` and `.` `.` `=` into `..=` only when they are glued (`at_composite2`), and
/// a token tree marks a punctuation mark `Joint` only when the next is glued to it and is
/// punctuation or a lifetime (`syntax_bridge`'s `is_single_token_op`), which a `macro_rules!`
/// matcher and a proc macro read. Told by the characters at the edge, not by kind, so it holds
/// for how any edition splits a token (a guarded string that is one token in this edition and
/// `#` then a string in another). A float's jointness is of its own text (`shortcuts`), and
/// whether anything else is glued (`f( x )`, `a + b`) is read only off the file's text: by
/// `stringify!` and `assert!` (`source_joined`), which no row's key covers
/// ([`super::expansion`] keys neither, and `blaze_item_hash::reads` refuses the read).
fn glues(left: Token<'_>, right: Token<'_>) -> bool {
    /// A character that is a punctuation mark of a token tree on its own.
    fn op(byte: u8) -> bool {
        matches!(
            byte,
            b'=' | b'<'
                | b'>'
                | b'!'
                | b'&'
                | b'|'
                | b'~'
                | b'@'
                | b'.'
                | b','
                | b';'
                | b':'
                | b'#'
                | b'$'
                | b'?'
                | b'+'
                | b'-'
                | b'*'
                | b'/'
                | b'%'
                | b'^'
        )
    }
    let doc = |kind: SyntaxKind| matches!(kind, SyntaxKind::INNER_DOC_COMMENT | SyntaxKind::OUTER_DOC_COMMENT);
    !doc(left.kind)
        && !doc(right.kind)
        && left.text.as_bytes().last().is_some_and(|&it| op(it))
        && right.text.as_bytes().first().is_some_and(|&it| it == b'\'' || op(it))
}

/// One hash being fed, and whether any token fed has `cfg` in it.
struct Feed {
    hasher: Hasher,
    cfg: bool,
}

impl Feed {
    fn new(domain: &'static str) -> Feed {
        Feed { hasher: Hasher::new(domain), cfg: false }
    }

    /// Feeds one piece of text: each of its tokens ([`Token::feed`]), then its end.
    fn piece(&mut self, text: &str) {
        tokens(text, |_, token| token.feed(self));
        self.end();
    }

    /// Ends a run of tokens: the word of `EOF`, which no token of a text is.
    fn end(&mut self) {
        self.hasher.update(&(SyntaxKind::EOF as u16).to_le_bytes());
    }

    /// Feeds a mark ([`CUT`]): a word no token's kind starts.
    fn mark(&mut self, mark: u32) {
        self.hasher.update(&mark.to_le_bytes());
    }

    fn kind(&mut self, kind: SyntaxKind) {
        self.hasher.update(&(kind as u16).to_le_bytes());
    }

    fn finish(self) -> (u128, bool) {
        (self.hasher.finish().fingerprint().0, self.cfg)
    }
}

/// Feeds the inner attributes and inner doc comments `node` holds, in order: of a file, a
/// list of items, or a body (its statement list's). A block whose text surely starts with
/// none feeds none and is not walked, so a lazy body stays unparsed and feeds what a parsed
/// one does; any other is walked, which parses a lazy one.
fn inner_attrs(node: view::Node<'_>, feed: &mut Feed) {
    if node.kind() == SyntaxKind::BLOCK_EXPR
        && !view::block_may_start_with_inner_attr(node.text(), true)
    {
        return;
    }
    for child in node.children_with_tokens() {
        let NodeOrToken::Node(child) = child else { continue };
        if child.kind() == SyntaxKind::STMT_LIST {
            inner_attrs(child, feed);
        } else if view::AnyAttr::cast(child).is_some_and(|attr| attr.kind().is_inner()) {
            feed.piece(child.text());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use span::Edition;
    use syntax::{SourceFile, view::HasModuleItem as _};

    use super::*;

    /// The rows of `text`, in the file's order (the file's own first), from a full parse or an
    /// outline.
    fn rows_of(text: &str, outline: bool) -> Vec<(u32, ItemSource)> {
        let parse = if outline {
            SourceFile::parse_outline(text, Edition::Edition2021)
        } else {
            SourceFile::parse(text, Edition::Edition2021)
        };
        let map = AstIdMap::from_view(parse.root());
        // alloc: a test's list
        rows(parse.root(), &map, &mut Vec::new())
    }

    fn index_of(text: &str, outline: bool) -> Vec<u8> {
        let parse = if outline {
            SourceFile::parse_outline(text, Edition::Edition2021)
        } else {
            SourceFile::parse(text, Edition::Edition2021)
        };
        make(parse.root(), &AstIdMap::from_view(parse.root()))
    }

    /// Rows: 0 the file, 1 `S`, 2 `impl S`, 3 `a`, 4 `b`, 5 `impl T for S`, 6 `c`, 7 `E`,
    /// 8 `V`, 9 `free`.
    fn text(a_body: &str, b_sig: &str, s_attr: &str) -> String {
        // alloc: a test's fixture
        format!(
            "{s_attr}\nstruct S;\n\
             impl S {{\n    fn a(&self) -> u32 {{ {a_body} }}\n    fn b({b_sig}) {{ 2; }}\n}}\n\
             impl T for S {{\n    fn c() {{ #![allow(unused)] 3; }}\n}}\n\
             enum E {{ V = 1 }}\n\
             fn free() -> [u8; {{ fn inner() {{ 0; }} 3 }}] {{ [0; 3] }}\n"
        )
    }

    const N: usize = 10;

    /// Which rows differ between two files of the same items.
    fn moved(a: &[(u32, ItemSource)], b: &[(u32, ItemSource)]) -> Vec<(usize, bool, bool, bool)> {
        assert_eq!((a.len(), b.len()), (N, N), "{a:?}\n{b:?}");
        a.iter()
            .zip(b)
            .enumerate()
            .filter(|(_, (a, b))| a.1 != b.1)
            .map(|(at, (a, b))| {
                (at, a.1.attrs != b.1.attrs, a.1.header != b.1.header, a.1.body != b.1.body)
            })
            // alloc: a test's list
            .collect()
    }

    /// The invariant: a body's edit moves that item's `body` and nothing else of any row:
    /// not its header, not its impl's header.
    #[test]
    fn a_body_edit_moves_only_that_body() {
        let before = rows_of(&text("1", "", "#[derive(Clone)]"), false);
        let after = rows_of(&text("1 + 41", "", "#[derive(Clone)]"), false);
        assert_eq!(moved(&before, &after), [(3, false, false, true)]);
    }

    /// The control of the above: a signature's edit moves the item's header and its impl's,
    /// and no other row; an attribute's moves its item's attrs and header.
    #[test]
    fn a_header_edit_moves_the_item_and_its_impl() {
        let before = rows_of(&text("1", "", "#[derive(Clone)]"), false);
        let after = rows_of(&text("1", "x: u8", "#[derive(Clone)]"), false);
        assert_eq!(moved(&before, &after), [(2, false, true, false), (4, false, true, false)]);
        let after = rows_of(&text("1", "", "#[derive(Clone, Copy)]"), false);
        assert_eq!(moved(&before, &after), [(1, true, true, false)]);
    }

    /// The invariant: a row is of its item's text alone: not of where the item is, and not of
    /// whether its bodies were parsed. An outline and a full parse make the same bytes.
    #[test]
    fn an_index_is_the_same_wherever_the_items_are_and_however_parsed() {
        let text = text("1", "", "");
        let full = index_of(&text, false);
        assert_eq!(full, index_of(&text, true), "an outline and a full parse");
        let rows = rows_of(&text, false);
        // alloc: a test's text
        let shifted = rows_of(&format!("\n\n// a comment\n{text}"), false);
        // The file's own row aside, which holds no position either.
        assert_eq!(moved(&rows, &shifted), []);
        let index = decode(&full).expect("the index this wrote");
        assert_eq!(index.len(), N);
        for (id, source) in rows {
            assert_eq!(index.get(ErasedFileAstId::from_raw(id)), Some(source));
        }
    }

    /// The invariant of the positions: each row's range is its node's range in the file, as
    /// the ast id map places it, read from the index alone; an outline places them as a full
    /// parse does. The control: the text shifted by a comment moves every range but the
    /// file's own start, and no row's hashes.
    #[test]
    fn an_index_places_each_item_where_its_file_has_it() {
        let text = text("1", "", "#[derive(Clone)]");
        for outline in [false, true] {
            let parse = if outline {
                SourceFile::parse_outline(text.as_str(), Edition::Edition2021)
            } else {
                SourceFile::parse(&text, Edition::Edition2021)
            };
            let map = AstIdMap::from_view(parse.root());
            let index = decode(&make(parse.root(), &map)).expect("the index this wrote");
            for (id, _) in rows_of(&text, outline) {
                let ast = ErasedFileAstId::from_raw(id);
                let want = map.get_erased(ast).text_range();
                assert_eq!(index.range(ast), Some(want), "row {id:#x}, outline {outline}");
            }
        }
        // alloc: a test's text
        let shifted = format!("// a comment\n{text}");
        let (a, b) = (index_of(&text, false), index_of(&shifted, false));
        let (a, b) = (decode(&a).unwrap(), decode(&b).unwrap());
        let mut moved = 0;
        for ((id, source), (_, shifted_source)) in a.rows().zip(b.rows()) {
            assert_eq!(source, shifted_source, "a row's hashes do not move");
            if a.range(id) != b.range(id) {
                moved += 1;
            }
        }
        assert_eq!(moved, N, "every range moves by the comment: the file's grows, the rest shift");
    }

    /// An inner attribute of a cut body is of the header (what collecting the item reads),
    /// lazy or parsed.
    #[test]
    fn an_inner_attribute_of_a_body_is_in_the_header() {
        let with = "impl T for S {\n    fn c() { #![allow(unused)] 3; }\n}\n";
        let without = "impl T for S {\n    fn c() { 3; }\n}\n";
        for outline in [false, true] {
            let (with, without) = (rows_of(with, outline), rows_of(without, outline));
            assert_ne!(with[1].1.header, without[1].1.header, "the impl's header");
            assert_ne!(with[2].1.attrs, without[2].1.attrs, "the fn's attrs");
        }
    }

    /// `plain` is said only of an impl whose assoc items no crate could read otherwise; the
    /// controls are each thing that takes a name's lookup.
    #[test]
    fn an_impl_is_plain_only_with_no_macro_and_built_in_attributes() {
        let plain = |assoc: &str| rows_of(&format!("impl S {{ {assoc} }}"), false)[1].1.plain;
        assert!(plain("#[inline] fn a() {} /// doc\n const C: u8 = 1; type T = u8;"));
        assert!(plain("#[rustc_const_stable(feature = \"x\")] #[cfg(unix)] fn a() {}"));
        assert!(!plain("m!();"));
        assert!(!plain("#[tokio::main] fn a() {}"));
        assert!(!plain("#[my_attr] fn a() {}"));
        assert!(!plain("#[cfg_attr(unix, my_attr)] fn a() {}"));
        assert!(!plain("fn a() { #![my_attr] }"));
        let rows = rows_of("impl S { #[cfg(unix)] fn a() {} }\nimpl S { fn b() { cfg!(unix); } }", false);
        assert!(rows[1].1.header_cfg && !rows[1].1.attrs_cfg && rows[2].1.attrs_cfg);
        assert!(!rows[3].1.header_cfg, "a body's text is not of the header");
    }

    #[test]
    fn a_previous_layout_of_a_file_index_is_refused_by_name() {
        let mut bytes = index_of(&text("1", "", ""), false);
        assert!(decode(&bytes).is_ok());
        bytes[8..12].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
        let why = decode(&bytes).expect_err("another layout");
        // alloc: a test's text
        assert!(why.contains(&format!("layout {}", LAYOUT + 1)), "{why}");
        // The control: bytes cut short are refused too, by their size.
        let whole = index_of(&text("1", "", ""), false);
        assert!(decode(&whole[..whole.len() - 1]).expect_err("cut").contains("rows in"));
    }

    /// The index lists each `impl`'s and `trait`'s assoc items in the order written, by the
    /// ast id each has in this file: an entry keyed by an `impl`'s text names
    /// an assoc item by its place in that order, and reads its id here. Two `impl`s written
    /// the same in one file have each its own ids, which is why no such entry holds one.
    #[test]
    fn an_index_lists_the_assoc_items_of_each_impl_in_the_order_written() {
        let twin =
            "impl S {\n    fn f() {}\n    const C: u8 = 1;\n    m!();\n    type T = u8;\n}\n";
        // alloc: a test's text
        let text = format!(
            "mod a {{\nstruct S;\n{twin}}}\nmod b {{\nstruct S;\n{twin}}}\n\
             trait Tr {{ fn g(); }}\n"
        );
        for outline in [false, true] {
            let rows = rows_of(&text, outline);
            let index = decode(&index_of(&text, outline)).expect("the index this wrote");
            // Rows: 0 the file, 1 `a`, 2 `S`, 3 the impl, 4..=7 its items, 8 `b`, 9 `S`,
            // 10 the impl, 11..=14 its items, 15 `Tr`, 16 `g`.
            assert_eq!(rows.len(), 17, "{rows:?}");
            let listed = |owner: usize| {
                let items = index.assoc_items(ErasedFileAstId::from_raw(rows[owner].0));
                // alloc: a test's list
                let listed: Vec<u32> =
                    (0..items.len()).map(|at| items.get(at).expect("within").into_raw()).collect();
                listed
            };
            let ids = |from: usize, to: usize| {
                // alloc: a test's list
                let ids: Vec<u32> = rows[from..=to].iter().map(|row| row.0).collect();
                ids
            };
            assert_eq!(listed(3), ids(4, 7));
            assert_eq!(listed(10), ids(11, 14));
            assert_eq!(listed(15), ids(16, 16));
            // The two impls are written the same (one header) and have other ids, as their
            // items do: the cause an entry of theirs must hold no id.
            assert_eq!(rows[3].1.header, rows[10].1.header);
            assert_ne!(rows[3].0, rows[10].0);
            assert!(listed(3).iter().all(|id| !listed(10).contains(id)));
            // An item that is no impl or trait lists none; past the last is none.
            assert!(index.assoc_items(ErasedFileAstId::from_raw(rows[2].0)).is_empty());
            let items = index.assoc_items(ErasedFileAstId::from_raw(rows[3].0));
            assert_eq!(items.get(4), None);
            assert_eq!(items.position(ErasedFileAstId::from_raw(rows[6].0)), Some(2));
            assert_eq!(items.position(ErasedFileAstId::from_raw(rows[11].0)), None);
        }
    }

    /// Which rows of `a` and `b`, two texts of the same items, differ, from a full parse or an
    /// outline: (row, attrs, header, body), the file's own row 0.
    fn differ(a: &str, b: &str, outline: bool) -> Vec<(usize, bool, bool, bool), Global> {
        let (a, b) = (rows_of(a, outline), rows_of(b, outline));
        assert_eq!(a.len(), b.len(), "{a:?}\n{b:?}");
        let mut out = Vec::new_in(Global);
        for (at, (a, b)) in a.iter().zip(&b).enumerate() {
            if a.1 != b.1 {
                out.push((at, a.1.attrs != b.1.attrs, a.1.header != b.1.header, a.1.body != b.1.body));
            }
        }
        out
    }

    const WRITTEN: &str = "/// Says hi.\n#[inline]\nfn f(x: u8) -> i8 {\n    let y = x + 1; // one more\n    \
                           -(y as i8)\n}\nconst C: i8 = -1;\nenum E { A = 1, B }\n\
                           impl S {\n    fn g(&self) -> &Self { self }\n}\n";

    /// The invariant: a row is of its item's tokens, not its text. Reindented, respaced and
    /// commented with plain comments, and moved down the file, every item is the same row,
    /// from a full parse and from an outline.
    #[test]
    fn a_respaced_commented_or_moved_item_is_the_same_row() {
        let respaced = "/// Says hi.\n# [ inline ]\nfn f( x : u8 )->i8\n{\n  /* plain */ let y = x+1;\n\n\
                        \t-( y as i8 )\n}\n// a plain comment\nconst C : i8 = -1 ;\n\
                        enum E {\n    A = 1,\n    B\n}\n\
                        impl S\n{\n    // a method\n    fn g( &self )-> &Self{self}\n}\n";
        let moved = "\n\n// cfg: a comment that names cfg\n/* and a block */\n\n/// Says hi.\n#[inline]\n\
                     fn f(x: u8) -> i8 {\n    let y = x + 1; // one more\n    -(y as i8)\n}\n\
                     const C: i8 = -1;\nenum E { A = 1, B }\nimpl S {\n    fn g(&self) -> &Self { self }\n}\n";
        for outline in [false, true] {
            assert_eq!(differ(WRITTEN, respaced, outline), [], "respaced, outline {outline}");
            assert_eq!(differ(WRITTEN, moved, outline), [], "moved, outline {outline}");
        }
        // A comment naming `cfg` is no `cfg` of the file.
        let rows = rows_of(moved, false);
        assert!(!rows[0].1.attrs_cfg && !rows[1].1.attrs_cfg, "{rows:?}");
        assert!(rows_of("#[cfg(unix)]\nfn f() {}", false)[1].1.attrs_cfg, "the control");
    }

    /// The control of the above: each change of a token moves its row, in the part of it
    /// the token is in. An operator, a literal's text (its spaces too), two tokens made one,
    /// two punctuation marks glued, a doc comment's text, a doc comment made a plain one.
    #[test]
    fn a_changed_token_moves_its_row() {
        let body = (1, false, false, true);
        let attrs = (1, true, true, false);
        let cases: [(&str, &str, (usize, bool, bool, bool)); 12] = [
            ("fn f() { a + b }", "fn f() { a - b }", body),
            ("fn f() { a b }", "fn f() { ab }", body),
            ("fn f() { x - -y }", "fn f() { x --y }", body),
            ("fn f() { a & &b }", "fn f() { a && b }", body),
            ("fn f() { x.. =y }", "fn f() { x..=y }", body),
            ("fn f() { \"a b\" }", "fn f() { \"a  b\" }", body),
            ("fn f() { r#\"a b\"# }", "fn f() { r#\"a  b\"# }", body),
            ("fn f() { 'a' }", "fn f() { 'b' }", body),
            ("/// a\nfn f() {}", "/// b\nfn f() {}", attrs),
            ("/// a\nfn f() {}", "///  a\nfn f() {}", attrs),
            ("/// a\nfn f() {}", "// a\nfn f() {}", attrs),
            ("#[doc = \"a\"]\nfn f() {}", "#[doc = \"b\"]\nfn f() {}", attrs),
        ];
        for (a, b, want) in cases {
            for outline in [false, true] {
                assert_eq!(differ(a, b, outline), [want], "{a:?} against {b:?}, outline {outline}");
            }
        }
        // A plain comment's text and the spaces between tokens move nothing.
        for (a, b) in [
            ("fn f() { 1 // one\n }", "fn f() { 1 // two\n }"),
            ("fn f() { a + b }", "fn f() {a+b}"),
            ("fn f() { x - -y }", "fn f() { x -  -y }"),
        ] {
            assert_eq!(differ(a, b, false), [], "{a:?} against {b:?}");
        }
        // An inner doc comment of a body is an inner attribute, as `#![doc]` is: of the
        // item's attributes, of its header (what collecting the item reads) and of its body.
        let inner = differ("fn f() {\n    //! a\n    1\n}", "fn f() {\n    //! b\n    1\n}", false);
        assert_eq!(inner, [(1, true, true, true)]);
        let inner = differ("fn f() { #![doc = \"a\"] 1 }", "fn f() { #![doc = \"b\"] 1 }", false);
        assert_eq!(inner, [(1, true, true, true)]);
    }

    /// `line!()` and `column!()` expand to a placeholder, never the place
    /// (`builtin::PLACE_PLACEHOLDER`), and what reads their value reads the file's text, which
    /// no key of an item covers: so an item that calls one is the same row wherever it is and
    /// however it is spaced, as any other. The control: which of the two it calls is a token.
    #[test]
    fn an_item_that_calls_line_is_the_same_row_where_it_moves() {
        let item = "fn f() -> u32 { line!() + column!() }\n";
        let moved = "\n\n\n// above\nfn f() -> u32 {\n        line!() + column!()\n}\n";
        assert_eq!(differ(item, moved, false), []);
        let other = "fn f() -> u32 { column!() + line!() }\n";
        assert_eq!(differ(item, other, false), [(1, false, false, true)]);
    }

    /// A derive or an attribute macro is given every token of its item; the row's `header`
    /// and `body` leave out an enum's discriminants and an `impl`'s methods. [`whole_tokens`]
    /// is of all of them, spaced or not.
    #[test]
    fn the_whole_item_is_every_token_of_it() {
        let (one, two) = ("enum E { A = 1 }", "enum E { A = 2 }");
        assert_eq!(rows_of(one, false)[1].1.header, rows_of(two, false)[1].1.header);
        assert_ne!(whole_tokens(one), whole_tokens(two), "a discriminant is of the whole");
        assert_eq!(whole_tokens(one), whole_tokens("enum E {\n    A = 1 // one\n}"));
        assert_ne!(whole_tokens("impl S { fn f() { 1 } }"), whole_tokens("impl S { fn f() { 2 } }"));
    }

    /// The cost: the hashes lex the item and feed each token to the hash, allocating nothing a
    /// token. An item of 1024 statements allocates what one of 16 does.
    #[test]
    fn hashing_an_item_allocates_nothing_a_token() {
        let used = |statements: usize| {
            let mut text = Vec::new_in(Global);
            text.extend_from_slice(b"/// doc\nfn f(x: i8) -> i8 {\n");
            for _ in 0..statements {
                text.extend_from_slice(b"    let y = x - -1; // a comment\n");
            }
            text.extend_from_slice(b"    x\n}\n");
            let text = std::str::from_utf8(&text).expect("ASCII");
            let parse = SourceFile::parse(text, Edition::Edition2021);
            let file = view::SourceFile::cast(parse.root()).expect("a file");
            let item = file.items().next().expect("the fn");
            let warm = item_source(item);
            let (again, used) = blaze_alloc_count::measure(|| item_source(item));
            assert_eq!(again, warm, "{statements} statements");
            used.allocations
        };
        let (small, big) = (used(16), used(1024));
        assert!(small <= 8, "an item of 16 statements: {small} allocations, budget 8");
        assert_eq!(big, small, "1024 statements against 16");
    }
}
