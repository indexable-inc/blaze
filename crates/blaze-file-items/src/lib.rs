//! A Rust source file's text to the list of items it declares: [`outline`], one pure function
//! with no tree kept and no database (docs/v2/plan/design/bottom.md, the `FileItems` row and
//! design point 2).
//!
//! The file is lexed and parsed once, and the parser's event stream is walked with a stack of
//! the nodes open at each step. Nothing of the parse outlives the call: no syntax tree, node
//! pointer or arena id is in an [`Outline`], only plain data (kinds, `u32` ranges, counts and
//! content hashes) in the caller's allocator.
//!
//! An outline lists, in source order, every item at any module-level nesting: the file's
//! items, the items of inline `mod m { .. }` bodies, of extern blocks, of traits and impls,
//! and the variants and fields of enums, structs and unions, each as a child of the entry it is
//! written in. Items inside a function body (or a `const`'s or `static`'s) are not listed:
//! they belong to the body's own later record, and the entry says whether its body holds any
//! item or macro call ([`ItemKind::Fn`]), so a later stage knows to look.
//!
//! # Positions
//!
//! An item's own [`Item::range`] is where it is in the file. Every other range of an item (its
//! name, signature, body, attributes, the ranges in its [`ItemKind`] and [`Visibility`]) is
//! relative to the start of that item (AGENTS.md rule 71, `item-relative-positions`): an edit
//! elsewhere in the file moves none of them, and a record made from one item's text can be
//! read without the file. [`Item::in_file`] turns one back into a range of the file. The
//! file's own inner attributes are relative to the file's start. The syntax errors
//! ([`Outline::errors`]) are the only other file-relative positions an outline holds: a
//! syntax error is about the file, not about an item.
//!
//! # Identity
//!
//! Each entry carries what the keys of later records are made from:
//!
//! - [`Item::disambiguator`]: the number of earlier siblings with the same kind and the same
//!   name (for an entry with no name: the same signature tokens outside its attributes, so
//!   whitespace runs count as one; for a tuple field: its position). Two builders of one text
//!   agree on it, and an edit outside the item's earlier equals does not move it. It is the
//!   index among equals the inherited layer's AST ids carry (`span::ast_id`).
//! - [`Item::hash`]: the [`ContentId`] of the item's tokens in [`ITEM_DOMAIN`], and
//!   [`Item::signature_hash`] of its tokens outside its body in [`SIGNATURE_DOMAIN`]. Both hash
//!   each token's text and whether it touches the token before it in the item, and no
//!   position, whitespace or non-doc comment; doc comments are tokens and are hashed. An
//!   entry's hash covers the entries written inside it (a module's covers its items), and its
//!   signature hash covers none of its body.
//!
//! # What the outline is the input of
//!
//! blaze-surface's `FileItems` record, the one a file's bytes key, is built from an
//! [`Outline`]: this crate knows nothing of that record and depends on no crate of the store.
//!
//! # Allocation
//!
//! Every allocation this crate makes is in the allocator the caller names. The lexer and the
//! parser it calls keep their own columns (tokens, the parser's input words, events), in the
//! worker's scratch (`blaze_mem::scratch`) and the global heap; all are freed or back on the
//! shelf before [`outline`] returns. Taking them into the caller's allocator is the parser's
//! change to make, not this crate's.
//!
//! It is on `std`, not `#![no_std]`: the parser it builds on (crates/parser) is on `std`
//! (`tracing`, `blaze_mem`'s per-thread scratch), so a crate that calls it is too.

#![feature(allocator_api)]

mod walk;

use std::alloc::Allocator;
use std::fmt;

pub use blaze_ids::ContentId;
pub use parser::{Edition, SyntaxKind};

/// The domain of [`Item::hash`]: an item's tokens, whole.
pub const ITEM_DOMAIN: &str = "blaze v2 file item 1";

/// The domain of [`Item::signature_hash`]: an item's tokens outside its body.
pub const SIGNATURE_DOMAIN: &str = "blaze v2 file item signature 1";

/// The outline of `text`, a source file of `edition`, in `alloc`: every item it declares at any
/// module-level nesting, in source order, and the syntax errors of the parse. It always
/// returns: what did not parse is in [`Outline::errors`], and an item-level stretch the parser
/// could not read is an [`ItemKind::Error`] entry.
///
/// The parse is Rust's syntax, with none of blaze's syntax extensions on
/// (`parser::Extensions::NONE`).
///
/// # Panics
///
/// When `text` is longer than `u32::MAX` bytes: every range is a `u32` pair, as the lexer's
/// offsets are.
pub fn outline<A: Allocator + Clone>(text: &str, edition: Edition, alloc: A) -> Outline<A> {
    walk::outline(text, edition, alloc)
}

/// A byte range, `start..end`: relative to the item it belongs to, or, for [`Item::range`] and
/// a [`SyntaxError`], to the file (see the crate's docs, "Positions").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Range {
    pub start: u32,
    pub end: u32,
}

impl Range {
    /// `start..end`.
    pub const fn new(start: u32, end: u32) -> Range {
        Range { start, end }
    }

    /// The number of bytes.
    pub const fn len(self) -> u32 {
        self.end - self.start
    }

    /// Whether it holds no byte.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// The range moved `by` bytes on.
    pub const fn shifted(self, by: u32) -> Range {
        Range { start: self.start + by, end: self.end + by }
    }

    /// The bytes of `text` it covers, as `text[start..end]`; panics as that does.
    pub fn of(self, text: &str) -> &str {
        &text[usize_of(self.start)..usize_of(self.end)]
    }
}

/// `u32` to `usize`: every target blaze builds has a `usize` of at least 32 bits.
fn usize_of(at: u32) -> usize {
    usize::try_from(at).expect("a u32 fits a usize on every target blaze builds")
}

/// An entry's place in [`Outline::items`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemIndex(pub u32);

impl ItemIndex {
    fn at(self) -> usize {
        usize_of(self.0)
    }
}

/// A run of [`Outline`]'s attributes, `start..end` in its list of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct AttrSpan {
    pub start: u32,
    pub end: u32,
}

/// One entry of an outline: an item, a variant or a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Item {
    pub kind: ItemKind,
    /// The entry it is written in: a module, trait, impl, extern block, enum, struct, union
    /// or variant. `None` for the file's own items.
    pub parent: Option<ItemIndex>,
    /// One past its last descendant in [`Outline::items`]: its descendants are the entries
    /// after it, up to here.
    pub end: ItemIndex,
    /// Where it is in the file: from its first outer attribute or doc comment (or its first
    /// token) to its last token.
    pub range: Range,
    /// Its name, as written (`r#` included); `None` for an impl, a `use`, an extern block, a
    /// macro call, a tuple field, `const _` and an error.
    pub name: Option<Range>,
    pub visibility: Visibility,
    /// Everything before its body: from its start (outer attributes included) to its body's
    /// start, or, with no body, to the end of its last token before a closing `;`. For a
    /// `const` or `static`, up to the `=`.
    pub signature: Range,
    /// The function's block, the module's, trait's, impl's or extern block's `{ .. }`, the
    /// enum's variants, the struct's, union's or variant's fields, the macro's or macro
    /// call's token tree, or a `const`'s or `static`'s value.
    pub body: Option<Range>,
    /// Its attributes and doc comments, outer then inner (a module's `#![..]`), in
    /// [`Outline::attrs`].
    pub attrs: AttrSpan,
    /// How many earlier siblings have the same kind and the same name (see the crate's docs,
    /// "Identity").
    pub disambiguator: u32,
    /// Its tokens' [`ContentId`] in [`ITEM_DOMAIN`].
    pub hash: ContentId,
    /// Its tokens outside its body, in [`SIGNATURE_DOMAIN`].
    pub signature_hash: ContentId,
}

impl Item {
    /// `range`, one of this item's item-relative ranges, as a range of the file.
    pub const fn in_file(&self, range: Range) -> Range {
        range.shifted(self.range.start)
    }

    /// The text of `range`, one of this item's item-relative ranges, in the file's `text`.
    pub fn text<'t>(&self, text: &'t str, range: Range) -> &'t str {
        self.in_file(range).of(text)
    }
}

/// What an entry is, and what each kind has that the others have not. Every range is relative
/// to the entry's start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemKind {
    /// `fn`. `body_holds_items`: its body holds an item or a macro call, at any depth.
    Fn { body_holds_items: bool },
    Struct,
    Enum,
    Union,
    Trait,
    /// `trait A = B + C;`.
    TraitAlias,
    Impl,
    /// `const`. `body_holds_items` as for [`ItemKind::Fn`], of its value.
    Const { body_holds_items: bool },
    /// `static`. `body_holds_items` as for [`ItemKind::Fn`], of its value.
    Static { body_holds_items: bool },
    TypeAlias,
    /// `mod m { .. }` (`inline`) or `mod m;`. `path`: the string literal of its outer
    /// `#[path = ".."]`, quotes and escapes as written.
    Mod { inline: bool, path: Option<Range> },
    /// `use`: the use tree (no `use`, no `;`).
    Use { tree: Option<Range> },
    /// `extern crate name as rename;`; the name is [`Item::name`].
    ExternCrate { rename: Option<Rename> },
    /// `extern "abi" { .. }`; its foreign items are its children. `abi`: `extern "abi"`.
    ExternBlock { abi: Option<Range> },
    /// `macro_rules! name { .. }`. `exported`: it has an outer `#[macro_export]`.
    MacroRules { exported: bool },
    /// `macro name(..) { .. }`, macros 2.0.
    MacroDef,
    /// A macro call in item position: its path and the token tree of its input.
    MacroCall { path: Option<Range>, input: Option<Range> },
    /// An enum's variant; its fields are its children. `discriminant`: the value after `=`.
    Variant { discriminant: Option<Range> },
    /// A struct's, union's or variant's field: named (a record field) or not (a tuple
    /// field). `ty`: its type; `default`: a default value after `=`.
    Field { ty: Option<Range>, default: Option<Range> },
    /// An item-level stretch the parser could not read as an item (its message is in
    /// [`Outline::errors`]): a stray token, an unmatched `}`, an attribute with no item, an
    /// inner attribute after the first item.
    Error,
    /// An item-level node of a kind this crate does not outline (`builtin # global_asm(..)`
    /// is the one the grammar has): listed with its syntax kind, never dropped.
    Unhandled { syntax: SyntaxKind },
}

/// An `extern crate`'s `as ..`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rename {
    /// `as name`: the name.
    Name(Range),
    /// `as _`.
    Underscore,
}

/// A visibility as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// None written.
    Private,
    /// `pub`.
    Pub,
    /// `pub(crate)`.
    Crate,
    /// `pub(super)`.
    Super,
    /// `pub(self)`.
    SelfOnly,
    /// `pub(in path)`: the path.
    In { path: Range },
    /// `pub(path)` with a path that is not `crate`, `super` or `self` and no `in`, which the
    /// parser takes and Rust refuses: the path, for the stage that refuses it to name.
    WithoutIn { path: Range },
}

/// An attribute or a doc comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Attr {
    /// Relative to its item's start (the file's, for the file's own).
    pub range: Range,
    /// `#![..]` or `//!`.
    pub inner: bool,
    pub kind: AttrKind,
}

/// What an attribute is, as far as an outline tells: `cfg` and `cfg_attr` are kept as their
/// tokens' range ([`Attr::range`]), never evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AttrKind {
    /// A doc comment, or `#[doc ..]`.
    Doc,
    /// `#[cfg(..)]`.
    Cfg,
    /// `#[cfg_attr(..)]`.
    CfgAttr,
    /// `#[path = ".."]`: the string literal, relative to the item's start.
    Path { literal: Range },
    /// `#[macro_export]`, with or without arguments.
    MacroExport,
    /// Any other.
    Other,
}

/// Which stage found a syntax error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorOrigin {
    Lexer,
    Parser,
}

/// A syntax error of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    /// Relative to the file's start: the token the lexer refused, or the empty range where
    /// the parser stopped expecting something.
    pub range: Range,
    pub origin: ErrorOrigin,
    /// Its message's bytes in the outline ([`Outline::message`]).
    message: Range,
}

/// What [`outline`] gives: plain data in the allocator `A`.
pub struct Outline<A: Allocator> {
    items: Vec<Item, A>,
    attrs: Vec<Attr, A>,
    file_attrs: AttrSpan,
    errors: Vec<SyntaxError, A>,
    /// The errors' messages, one after another, each copied from a `&str`.
    messages: Vec<u8, A>,
}

impl<A: Allocator> Outline<A> {
    /// Every entry, in source order: an entry comes before the entries written inside it.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The entry at `at`.
    pub fn item(&self, at: ItemIndex) -> &Item {
        &self.items[at.at()]
    }

    /// The entries written directly in `parent` (in the file itself, for `None`), in order.
    pub fn children(&self, parent: Option<ItemIndex>) -> Children<'_> {
        let (at, end) = match parent {
            None => (0, self.items.len()),
            Some(parent) => (parent.at() + 1, self.item(parent).end.at()),
        };
        Children { items: &self.items, at, end }
    }

    /// The attributes and doc comments of `item`.
    pub fn attrs(&self, item: &Item) -> &[Attr] {
        self.span(item.attrs)
    }

    /// The file's own inner attributes and doc comments (`#![..]`, `//!`).
    pub fn file_attrs(&self) -> &[Attr] {
        self.span(self.file_attrs)
    }

    fn span(&self, span: AttrSpan) -> &[Attr] {
        &self.attrs[usize_of(span.start)..usize_of(span.end)]
    }

    /// The syntax errors: the parser's, in the order it found them, then the lexer's.
    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }

    /// What `error`, one of [`Outline::errors`], says.
    pub fn message(&self, error: &SyntaxError) -> &str {
        let bytes = &self.messages[usize_of(error.message.start)..usize_of(error.message.end)];
        std::str::from_utf8(bytes).expect("a message is copied whole from a `&str`")
    }
}

/// The entries written directly in one entry or in the file ([`Outline::children`]).
pub struct Children<'o> {
    items: &'o [Item],
    at: usize,
    end: usize,
}

impl<'o> Iterator for Children<'o> {
    type Item = (ItemIndex, &'o Item);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.end {
            return None;
        }
        let item = &self.items[self.at];
        let index = ItemIndex(u32::try_from(self.at).expect("an outline's entries are counted in u32"));
        // A child's descendants follow it: the next child is past them.
        self.at = item.end.at();
        Some((index, item))
    }
}

impl<A: Allocator> PartialEq for Outline<A> {
    fn eq(&self, other: &Outline<A>) -> bool {
        self.items[..] == other.items[..]
            && self.attrs[..] == other.attrs[..]
            && self.file_attrs == other.file_attrs
            && self.errors[..] == other.errors[..]
            && self.messages[..] == other.messages[..]
    }
}

impl<A: Allocator> Eq for Outline<A> {}

impl<A: Allocator> fmt::Debug for Outline<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outline")
            .field("items", &&self.items[..])
            .field("attrs", &&self.attrs[..])
            .field("file_attrs", &self.file_attrs)
            .field("errors", &&self.errors[..])
            .finish_non_exhaustive()
    }
}
