//! Rust source text to signature records and body records, straight from the parser's events:
//! no syntax tree is built. One event-driven machine ([`machine`]) reads the parser's
//! steps once, keeps a stack of the nodes open and a stack of the values their children made,
//! and writes each node into its column when the node closes (post-order). Nothing recurses:
//! 100,000 nested parentheses or chained operators are 100,000 frames on a heap stack.
//!
//! # What is lowered
//!
//! - [`lower_signature`]: one item's text (blaze-file-items' range of it) to its signature:
//!   generics with bounds and defaults, where-clauses, parameters as patterns with types, the
//!   return type, qualifiers and ABI, `self` forms; fields with visibility, variants with
//!   discriminants (as anonymous constants), supertraits, the trait reference of an impl and
//!   its polarity, the self type; the associated items as entries (each lowered as an item of
//!   its own); attributes as (path, arguments) ranges, `cfg` and `cfg_attr` evaluated.
//! - [`lower_body`]: a fn's body or a const's or static's value, in post-order columns with
//!   locals, block scopes, labels and loop targets resolved here, and rustc's desugarings
//!   made ([`desugar`]): `for`, `while`, `?`, ranges, `.await` and `async fn`. `if let` chains
//!   stay an `if` over `let` expressions and `let else` a statement, as in rustc's HIR.
//! - [`lower_anon_const`]: an array length, const argument, discriminant or field default.
//!
//! A path that is not a local stays a path: [`resolve`] answers it through the names of the
//! world (blaze-scope's), returning `Need` for what is not known yet.
//!
//! # Keys (rule 21)
//!
//! - A signature record's key is [`signature_key`]: [`SIGNATURE_KEY_DOMAIN`] (which names this
//!   lowering's version and the parser's), the edition, whether the item has a body, and the
//!   item's signature text (blaze-file-items' `signature` range for a fn, const or static; the
//!   whole item for the rest). Its read list: the `cfg` atoms it asked ([`View::cfg_reads`]).
//! - A body record's key is [`body_key`]: [`BODY_KEY_DOMAIN`], the edition and the item's
//!   whole text. Its read list: the `cfg` atoms and the lang items its desugarings name
//!   ([`View::langs`]).
//! - An anonymous constant's is [`anon_const_key`]: the item's text and the constant's range.
//!
//! The item's `ItemKey` is where a record is filed, not part of its key: two items with equal
//! text have equal records. No range in a record is a file's: each is from the item's first
//! byte (rule 71), kept in side columns no key reads.
//!
//! # What blaze-typeck's form lacks
//!
//! [`typeck::to_typeck`] converts a body into blaze-typeck's `BodyView`, and refuses by name
//! what that form has no place for: `let` in a condition, a break with no target, lang-item
//! paths (every desugaring's), match and loop sources, coroutines and `yield`, `become`, raw
//! borrows, block kinds (`unsafe`, `const`, `try`), struct defaults `..`, box, range and rest
//! patterns, `b'x'` and `c"..."` literals, format arguments, macro calls, the shadowing links.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

mod columns;
pub mod desugar;
mod driver;
pub mod dump;
mod feed;
mod fmt;
pub mod form;
mod lit;
pub mod machine;
#[cfg(test)]
mod plant;
pub mod resolve;
pub mod typeck;

#[cfg(test)]
mod tests;

use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
pub use blaze_surface::{Edition, Range};

pub use crate::columns::{Columns, Head, LoweredRoot, View, open};
pub use crate::driver::{Expand, Expansion, FileStats, Sink, lower_file};
pub use crate::form::*;
pub use crate::machine::Mode;

/// The domain of [`signature_key`]: this lowering's version and the parser's.
pub const SIGNATURE_KEY_DOMAIN: &str = "blaze v2 lower signature 1, parser 1";
/// The domain of [`body_key`].
pub const BODY_KEY_DOMAIN: &str = "blaze v2 lower body 1, parser 1";
/// The domain of [`anon_const_key`].
pub const ANON_CONST_KEY_DOMAIN: &str = "blaze v2 lower anon const 1, parser 1";

/// What a `cfg` atom is in the build: blaze-surface's cfg set, asked one atom at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CfgAnswer {
    Yes,
    No,
    /// Not known yet: the lowering finishes its pass and returns [`Need`].
    Need,
}

/// The build's cfg set. `name` is the atom's name, `value` its string's contents (quotes
/// off, as written: an escape stays an escape).
pub trait Cfg {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer;
}

/// A lowering that met `cfg` atoms not known yet: their ranges, item-relative. Nothing else
/// of the pass is kept.
#[derive(Debug)]
pub struct Need<'a> {
    pub atoms: AVec<'a, Range>,
}

/// A finished lowering: its columns, read through [`Lowered::view`] or written as a record.
pub struct Lowered<'a> {
    pub columns: Columns<'a>,
}

impl<'a> Lowered<'a> {
    pub fn view(&self) -> View<'_> {
        self.columns.view()
    }

    /// The record: one blaze-format buffer, its root a [`LoweredRoot`].
    pub fn to_record<'b>(&self, alloc: Alloc<'b>) -> blaze_own::Vec<u8, Alloc<'b>> {
        self.columns.to_record(alloc)
    }
}

const fn edition_byte(edition: Edition) -> u8 {
    match edition {
        Edition::E2015 => 0,
        Edition::E2018 => 1,
        Edition::E2021 => 2,
        Edition::E2024 => 3,
    }
}

pub(crate) const fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

/// The key of an item's signature record: its signature text (`key_text`), the edition, and
/// whether a body follows the text (`fn f();` and `fn f() {}` share their text before it).
pub fn signature_key(key_text: &str, edition: Edition, has_body: bool) -> ContentId {
    Hasher::new(SIGNATURE_KEY_DOMAIN)
        .part(&[edition_byte(edition), has_body as u8])
        .part(key_text.as_bytes())
        .finish()
}

/// The key of an item's body record: its whole text and the edition.
pub fn body_key(item_text: &str, edition: Edition) -> ContentId {
    Hasher::new(BODY_KEY_DOMAIN).part(&[edition_byte(edition)]).part(item_text.as_bytes()).finish()
}

/// The key of an anonymous constant's body record: its text and where it starts in the item
/// (its record's ranges are the item's), and the edition.
pub fn anon_const_key(item_text: &str, range: Range, edition: Edition) -> ContentId {
    Hasher::new(ANON_CONST_KEY_DOMAIN)
        .part(&[edition_byte(edition)])
        .part(&range.start.to_le_bytes())
        .part(range.of(item_text).as_bytes())
        .finish()
}

/// Lowers one item's signature from its text (from its first attribute to its last token).
/// The text is parsed as an outline: a fn's body is never parsed.
///
/// # Panics
///
/// When `text` is longer than `u32::MAX` bytes.
pub fn lower_signature<'a>(
    text: &str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a>, Need<'a>> {
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse_outline(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Signature, edition, cfg, alloc);
    feed::feed(&mut m, &lexed, &output, 0);
    m.finish()
}

/// Lowers one item's body: a fn's block, or a const's or static's value. An item with none
/// (a trait method without a default) lowers to a record whose head says so.
///
/// # Panics
///
/// When `text` is longer than `u32::MAX` bytes.
pub fn lower_body<'a>(
    text: &str,
    edition: Edition,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a>, Need<'a>> {
    let pe = parser_edition(edition);
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::Body, edition, cfg, alloc);
    feed::feed(&mut m, &lexed, &output, 0);
    m.finish()
}

/// Lowers the anonymous constant at `range` of the item `text` (a [`ConstIdx`]'s range) as a
/// body of its own, its ranges still the item's.
pub fn lower_anon_const<'a>(
    text: &str,
    range: Range,
    edition: Edition,
    cfg: &mut dyn Cfg,
    alloc: Alloc<'a>,
) -> Result<Lowered<'a>, Need<'a>> {
    let pe = parser_edition(edition);
    let sub = range.of(text);
    let lexed = parser::LexedStr::new(pe, sub);
    let input = lexed.to_input(pe);
    let output = parser::TopEntryPoint::Expr.parse(input.tokens());
    let mut m = machine::Machine::new(text, Mode::AnonConst, edition, cfg, alloc);
    feed::feed(&mut m, &lexed, &output, range.start);
    m.finish()
}
