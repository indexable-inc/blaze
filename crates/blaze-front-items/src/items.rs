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
//! tokens and its trait's; of a `use`, its tree's tokens; of a macro call, its path; of an
//! `extern` block, its ABI's string. The scope is the container's id, but an `extern` block's items
//! are scoped as the block is (a block has no name, and makes no scope). There is no ordinal
//! of the file anywhere: inserting or removing an item moves no other item's id, except the
//! index of an item that names itself exactly as the inserted one does.
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
use syntax::{
    NodeOrToken, SyntaxKind, TextSize, WalkEvent,
    view::{self, AstNode as _, HasName as _},
};

use crate::{
    builtin::is_built_in,
    entry::{CFG, CFG_ATTR, DECLARED, EXPORTED, EntryBuf, Gate, ItemId, Kind, UNSPLIT},
    lines,
    table::{self, Family, Key},
};

/// The items table's family. Its layout changes with an entry's bytes, with which nodes have
/// entries, with what any hash covers and with how an id is made.
pub const ITEMS: Family = Family { number: 1, layout: 1, name: "items" };

const BASE_DOMAIN: &str = "blaze v2 front item base 1";
const ID_DOMAIN: &str = "blaze v2 front item id 1";
const HEADER_DOMAIN: &str = "blaze v2 front item header 1";
const BODY_DOMAIN: &str = "blaze v2 front item body 1";
const NAMES_DOMAIN: &str = "blaze v2 front item names 1";

/// In a feed, where the body was cut: no token's kind is this.
const CUT: u16 = 0xffff;
/// Where an item was taken out: its id follows.
const CHILD: u16 = 0xfffe;
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
        items: Vec::new(),
        lines: Vec::new(),
        lists: [const { Vec::new() }; 3],
    };
    walk.file(root)?;
    let Walk { mut items, lines: mut line_rows, lists, .. } = walk;
    let [impls, exports, mods] = lists;
    items.push((ItemId::IMPLS.0, impls));
    items.push((ItemId::EXPORTS.0, exports));
    items.push((ItemId::MODS.0, mods));
    line_rows.push((lines::LINE_STARTS, lines::line_starts(root.text())));
    // alloc: error path
    let items = table::write(ITEMS, items).map_err(|error| format!("{error}"))?;
    // alloc: error path
    let lines = table::write(lines::LINES, line_rows).map_err(|error| format!("{error}"))?;
    Ok(FileItems { items, lines })
}

/// One hash being fed tokens.
struct Feed {
    hasher: Hasher,
}

impl Feed {
    fn new(domain: &'static str, kind: SyntaxKind) -> Feed {
        let mut feed = Feed { hasher: Hasher::new(domain) };
        feed.mark(kind as u16);
        feed
    }

    fn token(&mut self, kind: SyntaxKind, joint: bool, text: &str) {
        self.hasher
            .update(&(kind as u16).to_le_bytes())
            .update(&[u8::from(joint)])
            .update(&(text.len() as u32).to_le_bytes())
            .update(text.as_bytes());
    }

    fn mark(&mut self, mark: u16) {
        self.hasher.update(&mark.to_le_bytes());
    }

    fn child(&mut self, id: ItemId) {
        self.mark(CHILD);
        self.hasher.update(&id.0);
    }

    fn finish(&self) -> Key {
        first_16(&self.hasher)
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
    let mut feed = Feed { hasher: Hasher::new(NAMES_DOMAIN) };
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
    items: Vec<(Key, Vec<u8>)>,
    lines: Vec<(Key, Vec<u8>)>,
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
    fn id(&mut self, scope: ItemId, kind: Kind, name: &str, names: Option<[Key; 2]>) -> ItemId {
        let mut base = Hasher::new(BASE_DOMAIN);
        base.update(&scope.0).update(&[kind as u8]).part(name.as_bytes());
        if let Some([first, second]) = names {
            base.update(&first).update(&second);
        }
        let base = first_16(&base);
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
        let id = self.id(scope, kind, &name, names);
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
        // An `extern` block has no name and makes no scope: its items are scoped as it is.
        let child_scope = if kind == Kind::ExternBlock { scope } else { id };
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
        own.body = Some(Feed::new(BODY_DOMAIN, value.kind()));
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
                        own.body = Some(Feed::new(BODY_DOMAIN, kind));
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
        let entry = EntryBuf {
            kind,
            flags: attrs.flags,
            container,
            header: own.header.finish(),
            body: own.body.as_ref().map(Feed::finish),
            body_span: own.body_span,
            tokens: own.tokens,
            name,
            gates: attrs.gates,
            children: own.children,
        };
        let range = node.text_range();
        let bytes = entry.write().map_err(|what| {
            // alloc: error path
            format!("items of a file: {what}, at byte {}", u32::from(range.start()))
        })?;
        // alloc: the file's rows, while its tables are made
        self.items.push((id.0, bytes));
        // alloc: the file's rows, while its tables are made
        self.lines.push((id.0, lines::row(range.start().into(), range.len().into(), &own.deltas)));
        Ok(())
    }
}
