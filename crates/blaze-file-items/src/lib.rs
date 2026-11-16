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
//! item or macro call ([`ItemDetail::Fn`](blaze_surface::ItemDetail::Fn)), so a later stage
//! knows to look.
//!
//! The value types an outline is made of (`Item`, `ItemDetail`, `Attr`, `SyntaxError`, ...)
//! are blaze-surface's, defined there once for the outline and the record it is kept as.
//!
//! # Positions
//!
//! An item's own [`Item::range`] is where it is in the file. Every other range of an item (its
//! name, signature, body, attributes, the ranges in its `ItemDetail` and its visibility) is
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
//! blaze-surface's `FileItems` record, the one a file's bytes key, is an [`Outline`] written by
//! [`Outline::to_file_items`]: it holds every value the outline holds, and its view answers
//! what the outline answers. The outline is the working form, alive for one task; the record
//! is what is kept and what every later stage reads.
//!
//! # Allocation
//!
//! Every allocation this crate makes is in the allocator the caller names. The lexer and the
//! parser it calls keep their own columns (tokens, the parser's input words, events), in the
//! worker's scratch (`blaze_mem::scratch`) and the global heap; all are freed or back on the
//! shelf before [`outline`] returns. Taking them into the caller's allocator is the parser's
//! change to make, not this crate's.
//!
//! The crate is `#![no_std]`: its collections are blaze-own's. The parser it calls
//! (crates/parser) is on `std` (`tracing`, `blaze_mem`'s per-thread scratch), which this crate
//! depends on but does not name.

#![no_std]
#![feature(allocator_api)]

mod walk;

use core::alloc::Allocator;
use core::fmt;

use blaze_own::Vec;
use blaze_surface::{
    Attr, AttrSpan, BuildError, Edition, FileItems, Item, ItemIndex, RecordBytes, SyntaxError,
};

pub use blaze_ids::ContentId;
pub use parser::SyntaxKind;

/// The domain of [`Item::hash`]: an item's tokens, whole.
pub const ITEM_DOMAIN: &str = "blaze v2 file item 1";

/// The domain of [`Item::signature_hash`]: an item's tokens outside its body.
pub const SIGNATURE_DOMAIN: &str = "blaze v2 file item signature 1";

/// The outline of `text`, a source file of `edition`, in `alloc`: every item it declares at any
/// module-level nesting, in source order, and the syntax errors of the parse. It always
/// returns: what did not parse is in [`Outline::errors`], and an item-level stretch the parser
/// could not read is an [`ItemDetail::Error`](blaze_surface::ItemDetail::Error) entry.
///
/// The parse is Rust's syntax, with none of blaze's syntax extensions on
/// (`parser::Extensions::NONE`).
///
/// # Panics
///
/// When `text` is longer than `u32::MAX` bytes: every range is a `u32` pair, as the lexer's
/// offsets are.
pub fn outline<A: Allocator + Clone>(text: &str, edition: Edition, alloc: A) -> Outline<A> {
    walk::outline(text, parser_edition(edition), alloc)
}

/// The schema's edition as the parser's.
fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

/// The parser's `SyntaxKind` of an [`ItemDetail::Unhandled`](blaze_surface::ItemDetail::Unhandled)
/// entry, whose number is this crate's parser's. Panics past the parser's last kind, which no
/// outline of this crate holds.
pub fn syntax_kind(syntax: u16) -> SyntaxKind {
    SyntaxKind::from(syntax)
}

/// What [`outline`] gives: plain data in the allocator `A`, the working form of a file's
/// `FileItems` record ([`Outline::to_file_items`]).
pub struct Outline<A: Allocator> {
    items: Vec<Item, A>,
    attrs: Vec<Attr, A>,
    file_attrs: AttrSpan,
    errors: Vec<SyntaxError, A>,
    /// The errors' messages, one after another, each copied from a `&str`.
    messages: Vec<u8, A>,
}

impl<A: Allocator> Outline<A> {
    /// The `FileItems` record of this outline of `text`, the file whose content is `file`, in
    /// `alloc`: every value the outline holds, and each entry's name's text. Refused only when
    /// the outline is not one of `text`, whose names it would cut.
    pub fn to_file_items<B: Allocator + Clone>(
        &self,
        text: &str,
        file: ContentId,
        alloc: B,
    ) -> Result<RecordBytes<B>, BuildError> {
        let parts = FileItems {
            file,
            text,
            items: &self.items,
            attrs: &self.attrs,
            file_attrs: self.file_attrs,
            errors: &self.errors,
            messages: &self.messages,
        };
        parts.write(alloc)
    }

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
        &self.attrs[span.to_usize()]
    }

    /// The syntax errors: the parser's, in the order it found them, then the lexer's.
    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }

    /// What `error`, one of [`Outline::errors`], says.
    pub fn message(&self, error: &SyntaxError) -> &str {
        let bytes = &self.messages[error.message.to_usize()];
        core::str::from_utf8(bytes).expect("a message is copied whole from a `&str`")
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
