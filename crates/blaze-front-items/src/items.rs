//! `items(file)`: a file's tree as a table of its items, by stable id, and the line part
//! beside it (end-state.md, "The front end as kernel steps", review point 4).
//!
//! # What an entry is made of
//!
//! Every hash is over **tokens**, never text: a token's kind, its text, and, where a macro
//! reads the stream (inside a token tree, and all through an unsplit item), whether it is
//! joined to the token after it. Whitespace and comments are no tokens; a doc comment is one
//! (it is an attribute). So a comment or whitespace edit anywhere leaves every entry's bytes
//! equal, unless it changes how the text lexes (`= =` to `==`), which is a token edit.
//!
//! An item's tokens are split three ways:
//!
//! - its **body** (a `fn`'s block, a `const`'s or `static`'s value, a variant's discriminant):
//!   the body hash;
//! - the **items taken out of it** (a module's items, an `impl`'s or `trait`'s assoc items, an
//!   `extern` block's, and every item written in its body or its header): each is its own
//!   entry under this one, and this one's hash holds only the child's id where it stood, so
//!   it covers the child's presence and place, not its content;
//! - the rest: the **header** hash, with the inner attributes of the body (they are the
//!   item's attributes).
//!
//! An item that may be a macro's input is **not split** ([`crate::entry::UNSPLIT`]): an item
//! under an attribute that is not built in (the old front end's `is_inert`, with one
//! refinement: a `cfg_attr` whose every written attribute is built in splits, since whichever
//! way its predicate goes the item is no macro's input), a macro call in item position, and a
//! macro's definition. Its one hash is of its whole token stream, spacing included.
//!
//! # Ids
//!
//! [`ItemId`] is the hash of (scope, kind, what names it), then of its index among the items
//! of exactly that first hash. What names an item: its name; of an `impl`, its self type's
//! tokens, its trait's and its gates (`cfg`, `cfg_attr`); of a `use`, its tree's tokens; of a
//! macro call, its path; of an `extern` block, its ABI's string. The scope is the container's
//! id, but an `extern` block's items are scoped as the block is (a block has no name, and
//! makes no scope), and an `impl`'s by what names the impl without its index (so its items'
//! ids do not move with it). There is no ordinal of the file anywhere: inserting or removing
//! an item moves no other item's id, except the index of an item that names itself exactly as
//! the inserted one does; for an `impl` of the same self type, trait and gates as later ones,
//! those impls' ids, and no id of an item in them.
//!
//! # Cost against the floor
//!
//! One preorder walk of the tree: `O(file)`, each token hashed once (a variant's tokens and a
//! body's inner attributes twice), one map lookup an item. The floor for any edit of a file is
//! "reparse the file"; this adds one hash pass. A comment edit changes 0 entries of
//! [`FileItems::items`] and the line part; a body edit changes that item's entry.

#![blaze::sans_io]

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};

use blaze_ids::Hasher;
use blaze_kernel::table::Builder;
use syntax::{
    NodeOrToken, SyntaxKind, TextSize, WalkEvent,
    builtin_attrs::is_built_in,
    view::{self, AstNode as _, HasName as _},
};

use crate::{
    entry::{
        CFG, CFG_ATTR, CHILD, DECLARED, EXPORTED, EntryBuf, Gate, HAS_BODY, ItemId, Kind, UNSPLIT,
        body_key,
    },
    lines,
    table::{self, Family, Key},
};

/// The items table, as an error names it.
pub const ITEMS: Family = Family { name: "items" };

const BASE_DOMAIN: &str = "blaze v2 front item base 1";
const ID_DOMAIN: &str = "blaze v2 front item id 1";
const HEADER_DOMAIN: &str = "blaze v2 front item header 1";
const NAMES_DOMAIN: &str = "blaze v2 front item names 1";
const RECORD_DOMAIN: &str = "blaze v2 front item body record 1";
const GATED_DOMAIN: &str = "blaze v2 front item impl gates 1";
const GROUP_DOMAIN: &str = "blaze v2 front item impl scope 1";

/// In a feed, where the body was cut: no token's kind is this.
const CUT: u16 = 0xffff;
/// Ends the inner attributes after a cut.
const END: u16 = 0xfffd;

/// A file's two parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileItems {
    /// The table of [`ITEMS`]: an [`crate::entry::Entry`] under each item's id and under
    /// [`ItemId::ROOT`], and the lists under [`ItemId::IMPLS`], [`ItemId::EXPORTS`] and
    /// [`ItemId::MODS`].
    pub items: Vec<u8>,
    /// The table of [`lines::LINES`]: under each item's id where it and its tokens are, and
    /// under [`lines::LINE_STARTS`] where the file's lines start.
    pub lines: Vec<u8>,
}

/// The items of the file (or the macro expansion) whose tree's root is `root`.
///
/// Pure: the same tree gives the same bytes. An error names what the tree holds that no table
/// can (two entries under one id, which is a hash collision).
pub fn file_items(root: view::Node<'_>) -> Result<FileItems, String> {
    // alloc: one file's rows, while its tables are made
    let mut walk = Walk {
        counts: BTreeMap::new(),
        items: Builder::new(),
        lines: Builder::new(),
        lists: [const { Vec::new() }; 3],
    };
    walk.file(root)?;
    let Walk { mut items, lines: mut line_rows, lists, .. } = walk;
    let [impls, exports, mods] = lists;
    items.part(&ItemId::IMPLS.0, &impls);
    items.part(&ItemId::EXPORTS.0, &exports);
    items.part(&ItemId::MODS.0, &mods);
    line_rows.part(&lines::LINE_STARTS, &lines::line_starts(root.text()));
    // alloc: error path
    let items = table::finish(ITEMS, items).map_err(|error| format!("{error}"))?;
    // alloc: error path
    let lines = table::finish(lines::LINES, line_rows).map_err(|error| format!("{error}"))?;
    Ok(FileItems { items, lines })
}

/// Tokens being fed: to a hash (a header, a name), or to a body's record ([`Body`]), whose
/// bytes are the ones a hash would be fed. A token is its kind (`u16`), whether it is joint
/// (`u8`), its length (`u32`) and its text; a mark is a `u16` no kind is; a child is
/// [`CHILD`] and its id.
struct Feed {
    out: Out,
}

enum Out {
    Hash(Hasher),
    Record(Vec<u8>),
}

impl Feed {
    fn new(domain: &'static str, kind: SyntaxKind) -> Feed {
        let mut feed = Feed { out: Out::Hash(Hasher::new(domain)) };
        feed.mark(kind as u16);
        feed
    }

    /// A body's record, which starts with the kind of the body's node.
    fn record(kind: SyntaxKind) -> Feed {
        // alloc: one body's record, kept as its part of the step's value
        let mut feed = Feed { out: Out::Record(Vec::new()) };
        feed.mark(kind as u16);
        feed
    }

    fn update(&mut self, bytes: &[u8]) {
        match &mut self.out {
            Out::Hash(hasher) => {
                hasher.update(bytes);
            }
            // alloc: one body's record, kept as its part of the step's value
            Out::Record(record) => record.extend_from_slice(bytes),
        }
    }

    fn token(&mut self, kind: SyntaxKind, joint: bool, text: &str) {
        self.update(&(kind as u16).to_le_bytes());
        self.update(&[u8::from(joint)]);
        self.update(&(text.len() as u32).to_le_bytes());
        self.update(text.as_bytes());
    }

    fn mark(&mut self, mark: u16) {
        self.update(&mark.to_le_bytes());
    }

    fn child(&mut self, id: ItemId) {
        self.mark(CHILD);
        self.update(&id.0);
    }

    /// The first 16 bytes of the hash; of a record, of the hash of its bytes. (A body's
    /// identity is its part's digest, which the kernel makes: the walk never asks this of one.)
    fn finish(&self) -> Key {
        match &self.out {
            Out::Hash(hasher) => first_16(hasher),
            Out::Record(record) => first_16(Hasher::new(RECORD_DOMAIN).update(record)),
        }
    }

    /// The record's bytes; of a hash, none.
    fn into_record(self) -> Option<Vec<u8>> {
        match self.out {
            Out::Hash(_) => None,
            Out::Record(record) => Some(record),
        }
    }
}

fn first_16(hasher: &Hasher) -> Key {
    hasher.finish().0[..16].try_into().expect("a content id is 32 bytes")
}

/// Whether `kind` is an operator character or a run of them: what a macro sees as a punct,
/// which has a spacing. A delimiter has none.
fn is_op(kind: SyntaxKind) -> bool {
    kind.is_punct()
        && !matches!(
            kind,
            SyntaxKind::L_PAREN
                | SyntaxKind::R_PAREN
                | SyntaxKind::L_CURLY
                | SyntaxKind::R_CURLY
                | SyntaxKind::L_BRACK
                | SyntaxKind::R_BRACK
        )
}

/// Whether `token` is joined to the token after it, inside the unit that ends at `end`: both
/// operators, nothing between them.
fn joint(token: view::Token<'_>, end: TextSize) -> bool {
    if !is_op(token.kind()) {
        return false;
    }
    let at = token.text_range().end();
    at < end
        && token
            .next_token()
            .is_some_and(|next| is_op(next.kind()) && next.text_range().start() == at)
}

/// Feeds every token of `node`, spacing included, to `feed`.
fn feed_all(node: view::Node<'_>, feed: &mut Feed) {
    let end = node.text_range().end();
    for element in node.descendants_with_tokens() {
        if let NodeOrToken::Token(token) = element
            && !token.kind().is_trivia()
        {
            feed.token(token.kind(), joint(token, end), token.text());
        }
    }
}

/// The hash of `node`'s tokens alone: what names an item that has no name.
fn names(node: Option<view::Node<'_>>) -> Key {
    let mut feed = Feed { out: Out::Hash(Hasher::new(NAMES_DOMAIN)) };
    match node {
        Some(node) => feed_all(node, &mut feed),
        None => feed.mark(END),
    }
    feed.finish()
}

/// `node`'s tokens' text, one after another.
fn joined(node: Option<view::Node<'_>>) -> String {
    // alloc: an item's name, kept in its entry
    let mut out = String::new();
    for element in node.into_iter().flat_map(|node| node.descendants_with_tokens()) {
        if let NodeOrToken::Token(token) = element
            && !token.kind().is_trivia()
        {
            out.push_str(token.text());
        }
    }
    out
}

/// The inner attributes and inner doc comments at the start of `node` (a body's statement
/// list, a list of items, a file), each given to `each`.
fn inner_attrs<'t>(node: view::Node<'t>, each: &mut impl FnMut(view::AnyAttr<'t>)) {
    for child in node.children() {
        if child.kind() == SyntaxKind::STMT_LIST {
            inner_attrs(child, each);
            return;
        }
        match view::AnyAttr::cast(child) {
            Some(attr) if attr.kind().is_inner() => each(attr),
            Some(_) => {}
            // Inner attributes come before everything else a list holds.
            None => return,
        }
    }
}

/// Whether `node` is an item in item position: taken out of what it is written in. A macro
/// call in expression, pattern or type position is its body's, and so is `asm!` as a
/// statement.
pub(crate) fn is_item(node: view::Node<'_>) -> bool {
    let kind = node.kind();
    if !view::Item::can_cast(kind) {
        return false;
    }
    match node.parent().map(|parent| parent.kind()) {
        Some(
            SyntaxKind::SOURCE_FILE
            | SyntaxKind::ITEM_LIST
            | SyntaxKind::ASSOC_ITEM_LIST
            | SyntaxKind::EXTERN_ITEM_LIST
            | SyntaxKind::MACRO_ITEMS,
        ) => true,
        Some(SyntaxKind::STMT_LIST | SyntaxKind::MACRO_STMTS) => kind != SyntaxKind::ASM_EXPR,
        _ => false,
    }
}

/// What an item's attributes say of it.
#[derive(Default)]
struct Attrs {
    flags: u8,
    gates: Vec<u8>,
}

#[derive(Default)]
struct Scan {
    /// An attribute that is not built in: it may be a macro.
    may_be_macro: bool,
    path: bool,
    export: bool,
}

fn scan(meta: view::Meta<'_>, out: &mut Scan) {
    match meta {
        view::Meta::CfgMeta(_) => {}
        view::Meta::CfgAttrMeta(it) => {
            for written in it.metas() {
                scan(written, out);
            }
        }
        other => match other.path() {
            None => out.may_be_macro = true,
            Some(path) => {
                let word = path.syntax().text();
                // `cfg_attr` written as a plain path is no `cfg_attr` the tree knows.
                if word == "cfg_attr" || !is_built_in(word) {
                    out.may_be_macro = true;
                } else if word == "path" {
                    out.path = true;
                } else if word == "macro_export" {
                    out.export = true;
                }
            }
        },
    }
}

fn push_gate(class: Gate, node: view::Node<'_>, gates: &mut Vec<u8>) {
    gates.push(class as u8);
    let count_at = gates.len();
    gates.extend_from_slice(&[0; 4]);
    let mut count = 0u32;
    for element in node.descendants_with_tokens() {
        if let NodeOrToken::Token(token) = element
            && !token.kind().is_trivia()
        {
            let text = token.text();
            gates.extend_from_slice(&(text.len() as u32).to_le_bytes());
            gates.extend_from_slice(text.as_bytes());
            count += 1;
        }
    }
    gates[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
}

fn gate(attr: view::AnyAttr<'_>, out: &mut Attrs) {
    let view::AnyAttr::Attr(attr) = attr else { return };
    let Some(meta) = attr.meta() else {
        // An attribute the parser could not read: nothing says it is built in.
        out.flags |= UNSPLIT;
        push_gate(Gate::Macro, attr.syntax(), &mut out.gates);
        return;
    };
    let mut seen = Scan::default();
    scan(meta, &mut seen);
    let class = match meta {
        view::Meta::CfgMeta(_) => {
            out.flags |= CFG;
            Some(Gate::Cfg)
        }
        view::Meta::CfgAttrMeta(_) => {
            out.flags |= CFG_ATTR;
            Some(Gate::CfgAttr)
        }
        _ if seen.may_be_macro => Some(Gate::Macro),
        _ if seen.path => Some(Gate::Path),
        _ => None,
    };
    if seen.may_be_macro {
        out.flags |= UNSPLIT;
    }
    if seen.export {
        out.flags |= EXPORTED;
    }
    if let Some(class) = class {
        push_gate(class, meta.syntax(), &mut out.gates);
    }
}

/// The gates and flags of the item `node`, whose inner attributes `list` holds.
fn attrs_of(node: view::Node<'_>, list: Option<view::Node<'_>>) -> Attrs {
    // alloc: empty until the item has a gate
    let mut out = Attrs::default();
    for child in node.children() {
        if let Some(attr) = view::AnyAttr::cast(child) {
            gate(attr, &mut out);
        }
    }
    if let Some(list) = list.filter(|list| *list != node) {
        inner_attrs(list, &mut |attr| gate(attr, &mut out));
    }
    out
}

fn varint(mut value: u32, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// An item's own tokens, as they are walked.
struct Own {
    header: Feed,
    body: Option<Feed>,
    in_body: bool,
    tokens: u32,
    body_span: (u32, u32),
    /// Where the last own token started.
    last: u32,
    /// Each own token's start, as its distance from the one before (the first from the
    /// item's start): the line part's.
    deltas: Vec<u8>,
    children: Vec<ItemId>,
}

impl Own {
    fn new(header: Feed, start: TextSize) -> Own {
        Own {
            header,
            body: None,
            in_body: false,
            tokens: 0,
            body_span: (0, 0),
            last: start.into(),
            // alloc: one item's line row, kept in the line part
            deltas: Vec::new(),
            // alloc: one item's children, copied into its entry
            children: Vec::new(),
        }
    }

    fn feed(&mut self) -> &mut Feed {
        match &mut self.body {
            Some(body) if self.in_body => body,
            _ => &mut self.header,
        }
    }

    fn token(&mut self, token: view::Token<'_>, joint: bool) {
        self.feed().token(token.kind(), joint, token.text());
        let at: u32 = token.text_range().start().into();
        varint(at - self.last, &mut self.deltas);
        self.last = at;
        self.tokens += 1;
    }

    fn child(&mut self, id: ItemId) {
        self.feed().child(id);
        self.children.push(id);
    }
}

struct Walk {
    /// The next index of each base hash: how many items of exactly that scope, kind and name
    /// came before. Looked up, never walked: its order reaches no output.
    counts: BTreeMap<Key, u32>,
    /// The parts of `items(file)`: an entry under each id, a body under each [`body_key`].
    items: Builder,
    lines: Builder,
    /// The ids of the impls, the exported macros and the declared modules, as written.
    lists: [Vec<u8>; 3],
}

/// Which entry [`Walk::finish`] keeps.
struct Head {
    id: ItemId,
    kind: Kind,
    container: ItemId,
    name: String,
}

/// What [`Walk::item`] found an item to be, before it is walked.
struct Shape<'t> {
    kind: Kind,
    name: String,
    /// What names it beside its name: of an `impl`, a `use`, `global_asm!`.
    names: Option<[Key; 2]>,
    body: Option<view::Node<'t>>,
    /// The node holding its inner attributes.
    list: Option<view::Node<'t>>,
    declared: bool,
}

fn text_of<'t>(name: Option<view::Name<'t>>) -> String {
    // alloc: an item's name, kept in its entry
    String::from(name.map_or("", |name| name.text()))
}

fn shape<'t>(item: view::Item<'t>) -> Shape<'t> {
    let mut shape = Shape {
        kind: Kind::Fn,
        // alloc: empty until named
        name: String::new(),
        names: None,
        body: None,
        list: None,
        declared: false,
    };
    match item {
        view::Item::Fn(it) => {
            shape.name = text_of(it.name());
            shape.body = it.body().map(|body| body.syntax());
            shape.list = shape.body;
        }
        view::Item::Const(it) => {
            shape.kind = Kind::Const;
            shape.name = match it.underscore_token() {
                // alloc: an item's name, kept in its entry
                Some(_) => String::from("_"),
                None => text_of(it.name()),
            };
            shape.body = it.body().map(|body| body.syntax());
        }
        view::Item::Static(it) => {
            shape.kind = Kind::Static;
            shape.name = text_of(it.name());
            shape.body = it.body().map(|body| body.syntax());
        }
        view::Item::Struct(it) => {
            shape.kind = Kind::Struct;
            shape.name = text_of(it.name());
        }
        view::Item::Union(it) => {
            shape.kind = Kind::Union;
            shape.name = text_of(it.name());
        }
        view::Item::Enum(it) => {
            shape.kind = Kind::Enum;
            shape.name = text_of(it.name());
        }
        view::Item::Trait(it) => {
            shape.kind = Kind::Trait;
            shape.name = text_of(it.name());
            shape.list = it.assoc_item_list().map(|list| list.syntax());
        }
        view::Item::TypeAlias(it) => {
            shape.kind = Kind::TypeAlias;
            shape.name = text_of(it.name());
        }
        view::Item::Impl(it) => {
            shape.kind = Kind::Impl;
            shape.names = Some([
                names(it.self_ty().map(|ty| ty.syntax())),
                names(it.trait_().map(|ty| ty.syntax())),
            ]);
            shape.list = it.assoc_item_list().map(|list| list.syntax());
        }
        view::Item::Module(it) => {
            shape.kind = Kind::Mod;
            shape.name = text_of(it.name());
            shape.list = it.item_list().map(|list| list.syntax());
            shape.declared = shape.list.is_none();
        }
        view::Item::Use(it) => {
            shape.kind = Kind::Use;
            shape.names = Some([names(it.use_tree().map(|tree| tree.syntax())), [0; 16]]);
        }
        view::Item::ExternCrate(it) => {
            shape.kind = Kind::ExternCrate;
            // alloc: an item's name, kept in its entry
            shape.name = String::from(it.name_ref().map_or("", |name| name.text()));
        }
        view::Item::ExternBlock(it) => {
            shape.kind = Kind::ExternBlock;
            // alloc: an item's name, kept in its entry
            shape.name = String::from(
                it.abi().and_then(|abi| abi.string_token()).map_or("", |abi| abi.text()),
            );
            shape.list = it.extern_item_list().map(|list| list.syntax());
        }
        view::Item::MacroCall(it) => {
            shape.kind = Kind::MacroCall;
            shape.name = joined(it.path().map(|path| path.syntax()));
        }
        view::Item::MacroRules(it) => {
            shape.kind = Kind::MacroRules;
            shape.name = text_of(it.name());
        }
        view::Item::MacroDef(it) => {
            shape.kind = Kind::MacroDef;
            shape.name = text_of(it.name());
        }
        view::Item::AsmExpr(it) => {
            shape.kind = Kind::Asm;
            shape.names = Some([names(Some(it.syntax())), [0; 16]]);
        }
    }
    shape
}

impl Walk {
    /// The id of the next item of `kind` under `scope` that `name` and `names` name.
    /// What names an item under `scope`: the first hash of its id.
    fn base(scope: ItemId, kind: Kind, name: &str, names: Option<[Key; 2]>) -> Key {
        let mut base = Hasher::new(BASE_DOMAIN);
        base.update(&scope.0).update(&[kind as u8]).part(name.as_bytes());
        if let Some([first, second]) = names {
            base.update(&first).update(&second);
        }
        first_16(&base)
    }

    fn id(&mut self, scope: ItemId, kind: Kind, name: &str, names: Option<[Key; 2]>) -> ItemId {
        self.indexed(Walk::base(scope, kind, name, names))
    }

    /// The id of the next item named by `base`: its index among the items of exactly that
    /// base, in the order written.
    fn indexed(&mut self, base: Key) -> ItemId {
        // alloc: one node of the map a distinct base
        let index = self.counts.entry(base).or_insert(0);
        let mut id = Hasher::new(ID_DOMAIN);
        id.update(&base).update(&index.to_le_bytes());
        *index += 1;
        ItemId(first_16(&id))
    }

    fn file(&mut self, root: view::Node<'_>) -> Result<(), String> {
        let mut attrs = attrs_of(root, None);
        // The file is split whatever its attributes are: a crate-level attribute macro is
        // named by its gate.
        attrs.flags &= !(UNSPLIT | EXPORTED);
        let mut own = Own::new(Feed::new(HEADER_DOMAIN, root.kind()), root.text_range().start());
        self.walk(root, None, false, ItemId::ROOT, ItemId::ROOT, &mut own)?;
        // alloc: empty: the file has no name
        let head = Head { id: ItemId::ROOT, kind: Kind::File, container: ItemId::ROOT, name: String::new() };
        self.finish(head, attrs, own, root)
    }

    /// The entry of the item `node`, written under `container` and scoped under `scope`, and
    /// of every item taken out of it.
    fn item(
        &mut self,
        node: view::Node<'_>,
        scope: ItemId,
        container: ItemId,
    ) -> Result<ItemId, String> {
        let item = view::Item::cast(node).expect("`is_item` took only an item's kinds");
        let Shape { kind, name, names, body, list, declared } = shape(item);
        let mut attrs = attrs_of(node, list);
        let whole = attrs.flags & UNSPLIT != 0
            || matches!(kind, Kind::MacroCall | Kind::MacroRules | Kind::MacroDef | Kind::Asm);
        if whole {
            attrs.flags |= UNSPLIT;
        }
        if kind != Kind::MacroRules {
            attrs.flags &= !EXPORTED;
        }
        if declared {
            attrs.flags |= DECLARED;
        }
        // An impl is named by its self type, its trait and its gates; its items are scoped by
        // that name, not by its id, which also holds its index among impls named the same.
        // So an impl written before others of the same name moves their ids and no id of an
        // item in them: a method's id is (self type, trait, gates, its own name), what
        // resolving it reads. (A hash of the impl's item names as the tie-break instead would
        // move the impl's id, the file's entry and the list of impls at every method added.)
        let (id, child_scope) = match (kind, names) {
            (Kind::Impl, Some([self_ty, trait_])) if !whole => {
                let mut gated = Hasher::new(GATED_DOMAIN);
                gated.update(&trait_).part(&attrs.gates);
                let base = Walk::base(scope, kind, &name, Some([self_ty, first_16(&gated)]));
                let mut group = Hasher::new(GROUP_DOMAIN);
                group.update(&base);
                (self.indexed(base), ItemId(first_16(&group)))
            }
            // An `extern` block has no name and makes no scope: its items are scoped as it is.
            (Kind::ExternBlock, _) => (self.id(scope, kind, &name, names), scope),
            _ => {
                let id = self.id(scope, kind, &name, names);
                (id, id)
            }
        };
        let list_of = match kind {
            Kind::Impl if !whole => Some(0),
            Kind::MacroRules if attrs.flags & EXPORTED != 0 => Some(1),
            Kind::Mod if declared && !whole => Some(2),
            _ => None,
        };
        if let Some(list) = list_of {
            // alloc: the file's list of ids, kept in the step's value
            self.lists[list].extend_from_slice(&id.0);
        }
        let mut own = Own::new(Feed::new(HEADER_DOMAIN, node.kind()), node.text_range().start());
        self.walk(node, body.filter(|_| !whole), whole, id, child_scope, &mut own)?;
        self.finish(Head { id, kind, container, name }, attrs, own, node)?;
        Ok(id)
    }

    /// The entry of the variant `variant` of the enum `container`, whose discriminant is
    /// `value`: the variant's tokens but the discriminant's are its header (and the enum's
    /// too), the discriminant its body.
    fn variant(
        &mut self,
        variant: view::Variant<'_>,
        value: view::Node<'_>,
        container: ItemId,
    ) -> Result<ItemId, String> {
        let name = text_of(variant.name());
        let id = self.id(container, Kind::Variant, &name, None);
        let mut attrs = attrs_of(variant.syntax(), None);
        // A variant is its enum's: an attribute that may be a macro is the enum's matter
        // (a derive's helper, mostly), named by its gate.
        attrs.flags &= !(UNSPLIT | EXPORTED);
        let mut header = Feed::new(HEADER_DOMAIN, SyntaxKind::VARIANT);
        let end = variant.syntax().text_range().end();
        let mut tokens = variant.syntax().preorder_with_tokens();
        while let Some(event) = tokens.next() {
            match event {
                WalkEvent::Enter(NodeOrToken::Node(node)) if node == value => {
                    header.mark(CUT);
                    tokens.skip_subtree();
                }
                WalkEvent::Enter(NodeOrToken::Token(token)) if !token.kind().is_trivia() => {
                    header.token(token.kind(), joint(token, end), token.text());
                }
                _ => {}
            }
        }
        let mut own = Own::new(header, value.text_range().start());
        own.body = Some(Feed::record(value.kind()));
        own.in_body = true;
        self.walk(value, None, false, id, id, &mut own)?;
        own.in_body = false;
        own.body_span = (0, own.tokens);
        self.finish(Head { id, kind: Kind::Variant, container, name }, attrs, own, value)?;
        Ok(id)
    }

    /// Feeds `node`'s own tokens to `own`: its header's, and its body's from where `body`
    /// starts to where it ends. Each item met in item position, and each variant's
    /// discriminant, is taken out: walked as its own entry under `me`, its id fed where it
    /// stood. With `whole`, nothing is taken out and every token is the header's, spacing
    /// included.
    fn walk(
        &mut self,
        node: view::Node<'_>,
        body: Option<view::Node<'_>>,
        whole: bool,
        me: ItemId,
        child_scope: ItemId,
        own: &mut Own,
    ) -> Result<(), String> {
        let end = node.text_range().end();
        // How many token trees the walk is inside: a macro reads what is in one.
        let mut trees = 0u32;
        let mut walk = node.preorder_with_tokens();
        while let Some(event) = walk.next() {
            match event {
                WalkEvent::Enter(NodeOrToken::Token(token)) => {
                    if !token.kind().is_trivia() {
                        own.token(token, (whole || trees > 0) && joint(token, end));
                    }
                }
                WalkEvent::Enter(NodeOrToken::Node(at)) => {
                    if at == node {
                        continue;
                    }
                    let kind = at.kind();
                    if kind == SyntaxKind::TOKEN_TREE {
                        trees += 1;
                    } else if whole {
                        // Nothing is taken out of a macro's input.
                    } else if Some(at) == body {
                        own.header.mark(CUT);
                        own.header.mark(kind as u16);
                        let header = &mut own.header;
                        inner_attrs(at, &mut |attr| feed_all(attr.syntax(), header));
                        own.header.mark(END);
                        own.body = Some(Feed::record(kind));
                        own.in_body = true;
                        own.body_span.0 = own.tokens;
                    } else if is_item(at) {
                        let child = self.item(at, child_scope, me)?;
                        own.child(child);
                        walk.skip_subtree();
                    } else if kind == SyntaxKind::CONST_ARG
                        && let Some(variant) = at.parent().and_then(view::Variant::cast)
                    {
                        let child = self.variant(variant, at, me)?;
                        own.child(child);
                        walk.skip_subtree();
                    }
                }
                WalkEvent::Leave(NodeOrToken::Node(at)) => {
                    if at != node && at.kind() == SyntaxKind::TOKEN_TREE {
                        trees -= 1;
                    } else if Some(at) == body && !whole {
                        own.in_body = false;
                        own.body_span.1 = own.tokens - own.body_span.0;
                    }
                }
                WalkEvent::Leave(NodeOrToken::Token(_)) => {}
            }
        }
        Ok(())
    }

    /// Keeps the entry and the line row of the item `head` names, which `node` is.
    fn finish(
        &mut self,
        head: Head,
        attrs: Attrs,
        own: Own,
        node: view::Node<'_>,
    ) -> Result<(), String> {
        let Head { id, kind, container, name } = head;
        let Own { header, body, body_span, deltas, children, .. } = own;
        let body = body.and_then(Feed::into_record);
        let entry = EntryBuf {
            kind,
            flags: attrs.flags | if body.is_some() { HAS_BODY } else { 0 },
            container,
            header: header.finish(),
            name,
            gates: attrs.gates,
            children,
        };
        let range = node.text_range();
        let bytes = entry.write().map_err(|what| {
            // alloc: error path
            format!("items of a file: {what}, at byte {}", u32::from(range.start()))
        })?;
        self.items.part(&id.0, &bytes);
        if let Some(body) = &body {
            self.items.part(&body_key(id), body);
        }
        let row = lines::row(range.start().into(), range.len().into(), body_span, &deltas);
        self.lines.part(&id.0, &row);
        Ok(())
    }
}
