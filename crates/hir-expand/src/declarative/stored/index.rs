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
//! The hashes are of text and of nothing a parse decides (a lazy node or a parsed one, where
//! the item is in its file), so an outline and a full parse of one text make the same bytes.
//!
//! The invariant: equal keys are equal bytes; a row is a function of its item's text alone.

use base_db::{Crate, Db, EditionedFileId};
use blaze_ids::{ContentId, Hasher};
use intern::Symbol;
use span::{AstIdMap, ErasedFileAstId, ROOT_ERASED_FILE_AST_ID};
use syntax::{
    NodeOrToken, SyntaxKind, ast,
    view::{self, AstNode as _, HasModuleItem as _},
};

use crate::HirFileId;

type R<T = ()> = Result<T, String>;

/// The layout of a file's index: part of its key and its second word. It changes with the
/// header, a row, which nodes have rows, or what any of the three hashes covers.
pub const LAYOUT: u32 = 1;

/// The family of a file's index, as the entry store names it.
pub const FAMILY: &str = "file index";

const INDEX_DOMAIN: &str = "blaze v2 file index";
const ATTRS_DOMAIN: &str = "blaze v2 item attrs";
const HEADER_DOMAIN: &str = "blaze v2 item header";
const BODY_DOMAIN: &str = "blaze v2 item body";

/// What an index starts with.
const MAGIC: [u8; 8] = *b"blzfidx\0";
/// Bytes before the rows: the magic, the layout, the count of rows.
const HEAD: usize = 16;
/// Bytes of a row: the ast id, whether it has a body, and the three hashes.
const ROW: usize = 56;

/// In a header's feed, where a body was cut: no piece is this long.
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
    /// Whether the text `attrs` covers has `cfg` in it: only then can what is made of the
    /// attributes differ with the crate's cfg.
    pub attrs_cfg: bool,
    /// The same of the text `header` covers.
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
        (self.bytes.len() - HEAD) / ROW
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

    /// The row of the item whose ast id is `ast`: a binary search of the rows.
    pub fn get(&self, ast: ErasedFileAstId) -> Option<ItemSource> {
        let want = ast.into_raw();
        let (mut low, mut high) = (0, self.len());
        // The bound: the range halves each turn.
        while low < high {
            let mid = low + (high - low) / 2;
            let (id, source) = self.row(mid);
            match id.cmp(&want) {
                std::cmp::Ordering::Equal => return Some(source),
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
            }
        }
        None
    }

    /// Every row, by ast id.
    pub fn rows(&self) -> impl Iterator<Item = (ErasedFileAstId, ItemSource)> + '_ {
        (0..self.len()).map(|at| {
            let (id, source) = self.row(at);
            (ErasedFileAstId::from_raw(id), source)
        })
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

/// The source hashes of the item `ast` of `file`: `None` for an item a macro wrote (its file
/// has no index: it is keyed by its expansion's inputs) and for a node with no row.
pub fn source(db: &Db, file: HirFileId, ast: ErasedFileAstId, krate: Crate) -> Option<ItemSource> {
    match file {
        HirFileId::FileId(file) => of(db, file, krate).get(ast),
        HirFileId::MacroFile(_) => None,
    }
}

/// The index of the file whose tree is `root` and whose ast ids are `map`, as its entry's
/// bytes.
pub fn make(root: view::Node<'_>, map: &AstIdMap) -> Vec<u8> {
    let mut rows = rows(root, map);
    rows.sort_unstable_by_key(|&(id, _)| id);
    if let Some(pair) = rows.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        panic!("file index: two items of one file with ast id {:#x}", pair[0].0);
    }
    // alloc: one file's index, kept as the query's value
    let mut out = Vec::with_capacity(HEAD + rows.len() * ROW);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&LAYOUT.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (id, source) in rows {
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
    let count = u32_at(bytes, 12) as usize;
    if bytes.len() != HEAD + count * ROW {
        // alloc: error path
        return Err(format!("a file index of {count} rows in {} bytes", bytes.len()));
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
    Ok(index)
}

/// Every row of the file, in the file's order: the file itself first.
fn rows(root: view::Node<'_>, map: &AstIdMap) -> Vec<(u32, ItemSource)> {
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
    out.push((ROOT_ERASED_FILE_AST_ID.into_raw(), file));
    if let Some(file) = view::SourceFile::cast(root) {
        for item in file.items() {
            item_rows(item, map, &mut out);
        }
    }
    out
}

fn item_rows(item: view::Item<'_>, map: &AstIdMap, out: &mut Vec<(u32, ItemSource)>) {
    out.push((map.ast_id_of_view::<ast::Item>(item).erase().into_raw(), item_source(item)));
    match item {
        view::Item::Module(it) => {
            for child in it.item_list().into_iter().flat_map(|list| list.items()) {
                item_rows(child, map, out);
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
                item_rows(child, map, out);
            }
        }
        view::Item::Impl(it) => assoc_rows(it.assoc_item_list(), map, out),
        view::Item::Trait(it) => assoc_rows(it.assoc_item_list(), map, out),
        view::Item::Enum(it) => {
            for variant in it.variant_list().into_iter().flat_map(|list| list.variants()) {
                let id = map.ast_id_of_view::<ast::Variant>(variant).erase().into_raw();
                let body = variant.const_arg().map(|it| it.syntax());
                out.push((id, node_source(variant.syntax(), body.as_slice(), body, None, false)));
            }
        }
        _ => {}
    }
}

fn assoc_rows(
    list: Option<view::AssocItemList<'_>>,
    map: &AstIdMap,
    out: &mut Vec<(u32, ItemSource)>,
) {
    for child in list.into_iter().flat_map(|list| list.assoc_items()) {
        item_rows(assoc_as_item(child), map, out);
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

/// Whether `attr` is an attribute no crate resolves as a macro: one word, built in or
/// `rustc_`'s, and not `cfg_attr`, which may write any attribute.
fn is_inert(attr: view::Attr<'_>) -> bool {
    let Some(path) = attr.path() else { return false };
    let word = path.syntax().text();
    word != "cfg_attr"
        && !word.is_empty()
        && word.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && (word.starts_with("rustc_")
            || crate::inert_attr_macro::find_builtin_attr_idx(&Symbol::intern(word)).is_some())
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

    let mut header = Feed::new(HEADER_DOMAIN);
    header.kind(node.kind());
    let text = node.text();
    let start = node.text_range().start();
    let mut at = 0;
    for cut in cuts {
        let range = cut.text_range();
        let (from, to) = (u32::from(range.start() - start) as usize, u32::from(range.end() - start) as usize);
        header.piece(&text[at..from]);
        header.mark(CUT);
        header.kind(cut.kind());
        inner_attrs(*cut, &mut header);
        header.mark(END);
        at = to;
    }
    header.piece(&text[at..]);

    let body = body.map(|body| {
        let mut hasher = Hasher::new(BODY_DOMAIN);
        hasher.update(&(body.kind() as u16).to_le_bytes()).update(body.text().as_bytes());
        hasher.finish().fingerprint().0
    });
    let (attrs, attrs_cfg) = attrs.finish();
    let (header, header_cfg) = header.finish();
    ItemSource { attrs, header, body, attrs_cfg, header_cfg, plain }
}

/// One hash being fed, and whether any text fed has `cfg` in it.
struct Feed {
    hasher: Hasher,
    cfg: bool,
}

impl Feed {
    fn new(domain: &'static str) -> Feed {
        Feed { hasher: Hasher::new(domain), cfg: false }
    }

    /// Feeds one piece of text: its length, then its bytes.
    fn piece(&mut self, text: &str) {
        self.cfg |= text.contains("cfg");
        self.hasher.update(&(text.len() as u32).to_le_bytes()).update(text.as_bytes());
    }

    /// Feeds a mark: a word no piece's length is.
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
    use span::Edition;
    use syntax::SourceFile;

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
        rows(parse.root(), &map)
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
}
