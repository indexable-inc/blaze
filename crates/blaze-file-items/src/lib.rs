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
//!   name (for an entry with no name: the same bytes of its signature from its first token
//!   outside its attributes; for a tuple field: its position). Two builders of one text agree
//!   on it, and an edit outside the item's earlier equals does not move it. It is the index
//!   among equals the inherited layer's AST ids carry (`span::ast_id`).
//! - [`Item::hash`]: the [`ContentId`] in [`ITEM_DOMAIN`] of the edition and the item's bytes,
//!   from its first token (an outer attribute's or doc comment's) to its last, and
//!   [`Item::signature_hash`] of the edition and the prefix of those bytes before its body:
//!   one hasher is fed the signature's bytes, read at the cut, then fed the rest, so no byte is
//!   hashed twice. An entry's hash covers the entries written inside it (a module's covers its
//!   items), and its signature hash covers none of its body.
//!
//! ## Why bytes, not tokens (decided 2026-11-15 by the coordinator of the rebuild)
//!
//! The digests are of bytes, not of the token stream. The cost accepted: an edit to only the
//! whitespace or a comment inside an item changes that item's digests, and the records keyed
//! by them are made again, for that one item. What it buys: a `fn` body that can hold no item
//! and no macro call is never lexed or parsed by the outline. Its end is found by
//! `blaze_lex::skip_body` (bracket depth over a structural mask, about 3.4 GB/s on one core)
//! and its bytes go to BLAKE3 as one slice (crate::skeleton). Positions stay byte offsets
//! relative to the item, which the diagnostics read anyway. The floor of an outline is then
//! one skip of the file plus one BLAKE3 pass over each byte times the number of entries
//! enclosing it.
//!
//! Measured 2026-11-15, one run on the owner's Mac, `examples/time.rs` over library/ (3,130
//! files, 57.0 MB, one thread): the outline took 1,005.7 ms before (token digests, bodies
//! skipped by the parser but lexed) and 769.6 ms after (74 MB/s). The floor: a skip of 57.0 MB
//! at 3,391.6 MB/s (blaze-lex's `cost`) is 16.8 ms, BLAKE3 over every byte once is 77.4 ms
//! (736 MB/s, `blaze_blake3`), 94.2 ms in all: the outline is 8.2x the floor. A sample of the
//! run puts about a sixth of the time in BLAKE3 (entries nested in an impl or a module are
//! hashed again inside it), a ninth in the token scans of the split and the skeleton, and the
//! rest in `LexedStr`, the parser and the walk over the text outside skipped bodies.
//!
//! `examples/huge.rs`, the same day, a 10.0 MB file of 745 library files as modules: a cold
//! outline in 120.0 ms on one thread and 14.1 ms on 18 (split 4.5 ms on one, parts 8.9 ms on
//! 18, assembly 0.8 ms); an edit to one part with every other part's outline in hand, 17.8 ms,
//! of which the digest of every part to look it up is 12.6 ms and the split 4.5 ms.
//!
//! # A file is outlined item by item
//!
//! 1. [`split_top_level`] cuts a file at its top-level item boundaries by a structural scan, no
//!    parse: after a `;` outside brackets, and after a `}` closing a top-level brace group
//!    (found by `blaze_lex::skip_body`, unlexed) when an item's first token follows.
//! 2. [`outline_item`] outlines one part from its bytes and the edition alone, so its result
//!    is a record keyed by the part's digest, made on any thread or found made before.
//! 3. [`assemble`] concatenates the parts' outlines: it moves file positions and indices by
//!    each part's base, and recounts the top-level disambiguators across parts.
//! 4. [`outline`] is the three in a row on one thread, tested equal to a whole-file walk on
//!    every file of library/; an edit to one item costs the scan, one part and the assembly.
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

mod header;
mod skeleton;
mod split;
mod walk;

use core::fmt;

use blaze_own::{AVec, Alloc};
use blaze_surface::{
    Attr, AttrSpan, BuildError, Edition, FileItems, Item, ItemIndex, RecordBytes, SyntaxError,
};

pub use blaze_ids::ContentId;
pub use header::{HeaderError, ImplHeader, impl_header};
pub use walk::Bodies;
pub use parser::SyntaxKind;

/// The domain of [`Item::hash`] and [`Item::signature_hash`]: the edition and an item's bytes,
/// whole or before its body.
pub const ITEM_DOMAIN: &str = "blaze v2 file item bytes 1";

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
///
/// A `fn` body is read only as far as it may hold an item or a macro call ([`Bodies::Skipped`]):
/// the syntax errors are those outside the bodies it leaves unread.
pub fn outline<'a>(text: &str, edition: Edition, alloc: Alloc<'a>) -> Outline<'a> {
    let parts = split_top_level(text, alloc);
    let mut outlines: AVec<'a, (u32, Outline<'a>)> = AVec::with_capacity_in(parts.len(), alloc);
    for part in &parts {
        outlines.push((part.start, outline_item(&text[part.to_usize()], edition, alloc)));
    }
    assemble(outlines.iter().map(|(base, outline)| (*base, outline)), alloc)
}

/// The outline of one part of a file ([`split_top_level`]), from its bytes and the edition
/// alone: positions are relative to the part's start, and the top-level disambiguators count
/// only the part's entries until [`assemble`] recounts them.
pub fn outline_item<'a>(part: &str, edition: Edition, alloc: Alloc<'a>) -> Outline<'a> {
    walk::outline(part, parser_edition(edition), Bodies::Skipped, alloc)
}

/// The whole file walked at once, with `fn` bodies read as `bodies` says: the reference
/// [`outline`] is tested equal to ([`Bodies::Skipped`]), and the reference that is to
/// ([`Bodies::Parsed`]).
pub fn outline_with<'a>(text: &str, edition: Edition, bodies: Bodies, alloc: Alloc<'a>) -> Outline<'a> {
    walk::outline(text, parser_edition(edition), bodies, alloc)
}

pub use split::{assemble, split_top_level};

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

/// What [`outline`] gives: plain data in the allocator `'a` names, the working form of a file's
/// `FileItems` record ([`Outline::to_file_items`]).
pub struct Outline<'a> {
    items: AVec<'a, Item>,
    attrs: AVec<'a, Attr>,
    file_attrs: AttrSpan,
    errors: AVec<'a, SyntaxError>,
    /// The errors' messages, one after another, each copied from a `&str`.
    messages: AVec<'a, u8>,
    /// Each top-level entry's sibling key (its parent, kind and name or head), in order: what
    /// its disambiguator counts.
    keys: AVec<'a, ContentId>,
}

impl Outline<'_> {
    /// The `FileItems` record of this outline of `text`, the file whose content is `file`, in
    /// `alloc`: every value the outline holds, and each entry's name's text. Refused only when
    /// the outline is not one of `text`, whose names it would cut.
    pub fn to_file_items<'b>(
        &self,
        text: &str,
        file: ContentId,
        alloc: Alloc<'b>,
    ) -> Result<RecordBytes<'b>, BuildError> {
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

impl PartialEq for Outline<'_> {
    fn eq(&self, other: &Outline<'_>) -> bool {
        self.items[..] == other.items[..]
            && self.attrs[..] == other.attrs[..]
            && self.file_attrs == other.file_attrs
            && self.errors[..] == other.errors[..]
            && self.messages[..] == other.messages[..]
            && self.keys[..] == other.keys[..]
    }
}

impl Eq for Outline<'_> {}

impl fmt::Debug for Outline<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outline")
            .field("items", &&self.items[..])
            .field("attrs", &&self.attrs[..])
            .field("file_attrs", &self.file_attrs)
            .field("errors", &&self.errors[..])
            .finish_non_exhaustive()
    }
}
