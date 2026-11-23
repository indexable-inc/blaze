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
//!   name (for an entry with no name: the same tokens of its signature from its first token
//!   outside its attributes, hashed in `blaze v2 file item head 2`; for a tuple field: its
//!   position). Two builders of one text agree on it, and an edit outside the item's earlier
//!   equals does not move it; an `impl` inserted before its twin (one with the same head
//!   tokens) moves the twin's, and so its chain (tests/outline/token_keys.rs). It is the index among
//!   equals the inherited layer's AST ids carry (`span::ast_id`).
//! - [`Item::signature_hash`] and [`Item::hash`]: a Merkle step over the item's tokens, from
//!   its first (an outer attribute's or doc comment's) to its last, cut where the signature ends
//!   (before the body, a direct `=` or a direct `;`). The item's own text is runs between its
//!   child entries (the runs before, between and after the children, empty ones too, and the
//!   run that holds the cut as two runs, cut there); each child entry is fed as the tag 1 and
//!   the child's own `hash` (32 bytes). A run is fed as the tag 2, its count of tokens
//!   (LEB128), then per token its `SyntaxKind` as a little-endian u16, the top bit set when it
//!   is joined to the next token (`parser::token_key::joined`: punctuation right against
//!   punctuation or a lifetime, so `&&` and `& &` differ), and unless it is punctuation its
//!   lexeme's length (LEB128) and bytes. Whitespace and plain comments are no tokens; a doc
//!   comment is one (its text is a `#[doc]`). The signature hash, in [`SIGNATURE_DOMAIN`], takes
//!   the edition (one byte), then the runs and children before the cut; the hash, in
//!   [`ITEM_DOMAIN`], takes the signature hash (32 bytes), then the runs and children after
//!   it. So every token is compressed once, by its innermost entry's one digest, and each
//!   digest is one short input that can be hashed side by side with others (crate::digest); an
//!   entry's hash changes whenever any token under it does (a module's covers its items) and
//!   never for a comment or a reformat; an edit to a child rehashes the child's own tokens and
//!   the 33 bytes it stands for in each ancestor; and the signature hash covers none of the
//!   body. (Until 2026-11-16 the signature hash was one hasher read at the cut and the hash the
//!   same hasher read at the end, in `merkle 1`; it was split so that no prefix is compressed
//!   twice when hashed side by side.)
//! - The exception (keys cover every input, rule 21): an entry whose own tokens call a macro or
//!   carry an attribute that is no inert built-in one ([`ItemFlags::MACRO_CALL`], its children's
//!   tokens not counted) has its runs fed by bytes, the tag 0, the length as a u64 and the bytes
//!   (`Hasher::part`): `stringify!`, `line!`, `column!`, a proc macro's `Span::source_text` and
//!   the lines its spans print read the layout tokens leave out. Its children are fed by their
//!   digests as ever, so a comment in a plain `fn` beside a `#[derive]` moves only that `fn`.
//!
//! ## Why tokens (decided 2026-11-18 by the coordinator of the rebuild, lane K1)
//!
//! From 2026-11-15 to 2026-11-18 the digests were of bytes: a comment or a reformat inside an
//! item made every record keyed by it again, the item's head and body check among them, and the
//! edit loop's commonest edit cost a body check for nothing. The tokens come from the one lex
//! the walk already made (its columns; crate::digest `Lexed`), so a run costs one pass over its
//! tokens' kinds and lexemes on top of the BLAKE3 the bytes cost; punctuation feeds two bytes
//! where its text is one or two. Positions in records do not survive a trivia edit by the key
//! alone: what reads them is in the crate's users (blaze-cli `engine_bodies`, where a finding
//! is a token place, `parser::token_key::TokenPlace`). The numbers below are of the byte digests;
//! the token digests' are not yet measured (examples/time.rs is the run that measures them).
//!
//! Since 2026-11-16 the text is lexed once, whole, and the item grammar itself decides what a
//! `fn` body is and leaves it unparsed (crate::walk::rows; `Bodies::Skipped`): the path before
//! it cut bodies out of the text unlexed by a reader of its own (`blaze_lex::skip_body` after
//! "the first `{` after `fn` and a name"), and lost what the cut held where the grammar reads
//! no `fn` body there (tests/rows.rs). The numbers below up to that date are that path's.
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
//! After the Merkle step, the same day: library/ in 694.6 ms on one thread (82 MB/s); the
//! floor 16.8 ms of skip and 72.9 ms of BLAKE3 (782 MB/s), 89.7 ms, so 7.7x. On the 10.0 MB
//! file, with [`resplit`] and the previous text in hand, an edit to one part costs 0.94 ms:
//! the resplit 0.24 ms (its compare alone 0.224 ms, 44.8 GB/s; 3,204 bytes scanned and
//! digested again), the outline of the one changed part 0.05 ms, the assembly 0.63 ms (a copy
//! of every entry, the one file-sized step left). Its floor, the compare and the changed
//! part's outline, is 0.27 ms. The split and digest of the whole file take 9.99 ms on one
//! thread and 1.58 ms on 18 ([`speculate`] per chunk 0.59 ms, [`join`] 0.19 ms with 5,311
//! bytes scanned again at the seams, the digests 0.78 ms).
//!
//! 2026-11-16, with the entry flags, [`Assembled`] in place of the copy, the faster
//! `blaze_blake3` and the digests split at the cut and hashed side by side (crate::digest):
//! library/ in 671.6 ms on one thread (85 MB/s), against 710.3 to 747.8 ms for the same tree
//! with digests streamed one entry at a time, rounds interleaved; 59.3 ms on 18 threads.
//! BLAKE3 over every byte is now 24.1 ms (2,366 MB/s), so the floor is about 41 ms and the
//! outline 16x it: the parser and the walk are what is left. The 10.0 MB file: cold 104.8 ms
//! on one thread, 8.8 ms on 18; an edit to one part 0.31 ms (resplit 0.24 ms, the part's
//! outline 0.04 ms, assembly 0.02 ms) against a floor of about 0.27 ms. Its parts' digests
//! gain nothing side by side (4.59 ms against 4.66 ms one by one): a part is kilobytes long,
//! and one input that long already fills the vector lanes chunk by chunk.
//!
//! # A file is outlined item by item
//!
//! 1. [`split_top_level`] cuts a file at its top-level item boundaries by a structural scan, no
//!    parse: after a `;` outside brackets, and after a `}` closing a top-level brace group
//!    (found by `blaze_lex::skip_body`, unlexed) when an item's first token follows.
//! 2. [`outline_item`] outlines one part from its bytes and the edition alone, so its result
//!    is a record keyed by the part's digest, made on any thread or found made before.
//! 3. [`Assembled`] puts the parts' outlines together without copying them: an entry is moved
//!    by its part's base when read, and the top-level disambiguators recounted across parts
//!    are a side table of the few that differ. An edit to one item costs the scan, one part,
//!    and a step a part and a probe a top-level entry to assemble.
//! 4. [`outline`], the cold path on one thread, reads the whole file in one pass: one lex, the
//!    tokens in columns the item grammar reads in place, a sink that writes each row as its
//!    item closes (no list of events, no tree), then every entry's digests in fixed batches.
//!    1 to 3 are tested equal to it (tests/outline.rs), and it to the outline with every body
//!    parsed (examples/oracle.rs over library/ and tempo's packages). 2026-11-16, library/
//!    on one thread: 360.5 ms (158 MB/s) beside 696.4 ms for the path it replaced.
//! 5. [`outline_tokens`] is 4 for a text written from tokens (an expansion rendered): the
//!    writer hands the tokens over ([`TokenColumns`]) and nothing is lexed.
//!
//! # Flags
//!
//! Each entry carries [`ItemFlags`] read from its own tokens in the one pass that fills the
//! token columns (crate::flags): `unsafe`, foreign code, inline assembly, and a macro call not
//! yet expanded. A body's tokens are read for them with the rest.
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
//! What an outline keeps is in the allocator the caller names, at its exact size and copied
//! there once. What it only works in (the outline as the sink grows it, the token columns, the
//! sink's stacks, the digests' key counts) is the worker's scratch (`blaze_mem::scratch`, on
//! the global heap) kept at its high-water mark, like the lexer's and the parser's columns
//! (tokens, input words, events); all are back on the shelf before [`outline`] returns. In a
//! bump frame the kept columns, growing by turns, would copy each other at each doubling, and
//! the working set would sit beneath them. Bytes per item in the frame, `scratch` example,
//! 2026-11-16 (before: each part's outline, then assembled and copied, all in the frame):
//!
//! | input                                   | items  | before   | after   | time before, after  |
//! |-----------------------------------------|--------|----------|---------|---------------------|
//! | 1,000 one-line `fn`s                    | 1,000  | 1,713 B  | 220 B   | 0.63 ms, 0.53 ms    |
//! | library/core, 299 files                 | 13,428 | 1,961 B  | 358 B   | 59.96 ms, 57.66 ms  |
//! | tempo's tt_2d_pool.rs, 315 kB           | 359    | 1,850 B  | 213 B   | 3.72 ms, 3.70 ms    |
//!
//! The floor is the kept outline, 172 B an item and its attributes, keys and errors: library/
//! core's 358 B is its attributes (`#[stable]`, `#[inline]`, ...), 24 B each.
//!
//! The crate is `#![no_std]`: its collections are blaze-own's. The parser it calls
//! (crates/parser) is on `std` (`tracing`, `blaze_mem`'s per-thread scratch), which this crate
//! depends on but does not name.

#![no_std]
#![feature(allocator_api)]
#![feature(portable_simd)]

// `thread_local!`: the worker's scratch outline (crate::walk).
extern crate std;

// D189's surface pass: a body call's path and input, a block `use`'s bindings, by tokens.
pub mod body_paths;
mod chain;
mod digest;
mod flags;
mod header;
mod names_digest;
pub mod nested;
mod split;
mod walk;

use core::fmt;

use blaze_own::{AVec, Alloc};
use blaze_surface::{
    Attr, AttrSpan, BuildError, Edition, FileItems, Item, ItemFlags, ItemIndex, ItemKind,
    RecordBytes, SyntaxError,
};

pub use blaze_ids::ContentId;
pub use chain::{CHAIN_DOMAIN, chain_at, head_of, item_chain};
pub use header::{GenericsError, HeaderError, ImplHeader, declared_lifetimes, impl_header};
pub use walk::Bodies;
pub use parser::{Extensions, SyntaxKind};

/// The domain of [`Item::hash`]: the Merkle step over an item's signature digest, the rest of
/// its own tokens (or bytes) and its children's digests (crate docs, "Identity"). `3` since the
/// runs are tokens.
pub const ITEM_DOMAIN: &str = "blaze v2 file item merkle 3";

/// The domain of [`Item::signature_hash`]: the edition, an item's own tokens (or bytes) before
/// its body and the digests of its children there (crate docs, "Identity"). `2` since the runs
/// are tokens.
pub const SIGNATURE_DOMAIN: &str = "blaze v2 file item signature 2";

/// The domain of an entry's names digest ([`Outline::names_digest`]): what name resolution can
/// observe of it without its types (crate::names_digest).
pub const NAMES_DOMAIN: &str = "blaze v2 file item names 1";

/// The domain of [`Outline::surface`] (the drive's `Surface` part). `v2` since 2026-11-16: the
/// surface hashes each entry's names digest, no longer its signature digest, so a surface of
/// `v1` is never compared with one of `v2` and every record keyed by one is made again. `v3`
/// since names-attr-4: it also hashes the file's inner attributes (`Outline::file_names`), which
/// the name solver reads (`#![register_tool]`, `#![no_std]`, a module's `#![cfg]`); a `v2`
/// surface is never compared with a `v3` one.
pub const SURFACE_DOMAIN: &str = "blaze-cli surface v3";

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
    walk::rows::outline::<SideBySide, ByTokens>(text, parser_edition(edition), Bodies::Skipped, alloc)
}

/// [`outline`], each entry's text fed to its digests as `F` says: [`ByTokens`] is [`outline`];
/// [`ByBytes`], every entry by its bytes, is the control that puts trivia back in the keys
/// (tests/outline/token_keys.rs).
pub fn outline_fed_by<'a, F: Feed>(text: &str, edition: Edition, alloc: Alloc<'a>) -> Outline<'a> {
    walk::rows::outline::<SideBySide, F>(text, parser_edition(edition), Bodies::Skipped, alloc)
}

/// The outline of `text`, a text written from tokens (an expansion rendered), read back with no
/// lexer: `tokens` are its tokens as its writer has them, one [`TokenColumns::push`] each, in
/// order. `edition` is the file's, as [`outline`]'s is; each token's own is in its word.
///
/// It is [`outline`]`(text)` in every entry, key, digest, attribute, flag and parser finding
/// when the tokens are what the lexer makes of `text` (examples/oracle.rs `--tokens` proves it
/// over library/ and tempo's packages; tests/rows.rs on the forms where joint tokens decide the
/// parse). The lexer's findings are not in it: nothing is lexed.
///
/// Refused, by name ([`TokensRefused`]): a text longer than `u32::MAX` bytes, a token that
/// ends past the text or is cut inside one of its characters.
pub fn outline_tokens<'a>(
    text: &str,
    tokens: &TokenColumns<'_>,
    edition: Edition,
    alloc: Alloc<'a>,
) -> Result<Outline<'a>, TokensRefused> {
    walk::rows::outline_tokens::<SideBySide, ByTokens>(text, tokens, parser_edition(edition), alloc)
}

pub use walk::rows::{TokenColumns, TokensRefused};

/// The outline of an expansion's token record (drive.md D122 rule 3), with no text: names are
/// lexemes, every extent a `TokenRange`, every digest the record's token digest
/// (crate::walk::token_outline). [`outline_tokens`], its text and token columns, goes once its
/// callers read token records.
pub use walk::token_outline::{TokenEntry, TokenOutline, TokenOutlineRefused, outline_token_record};

/// The outline of one part of a file ([`split_top_level`]), from its bytes and the edition
/// alone: [`outline`] of the part's text, so positions are relative to the part's start, and
/// the top-level disambiguators count only the part's entries until [`Assembled`] recounts
/// them.
pub fn outline_item<'a>(part: &str, edition: Edition, alloc: Alloc<'a>) -> Outline<'a> {
    outline(part, edition, alloc)
}

/// [`outline`] with `fn` bodies read as `bodies` says: [`Bodies::Skipped`] is [`outline`], and
/// [`Bodies::Parsed`], every body parsed with the rest, is the definition it is tested equal to
/// in every row (tests/outline.rs `skipping_bodies_outlines_as_parsing_them`; examples/oracle.rs
/// over the corpora).
pub fn outline_with<'a>(text: &str, edition: Edition, bodies: Bodies, alloc: Alloc<'a>) -> Outline<'a> {
    walk::rows::outline::<SideBySide, ByTokens>(text, parser_edition(edition), bodies, alloc)
}

/// [`outline_with`] with explicit blaze syntax extensions ([`Extensions`]) enabled on the parser.
pub fn outline_with_extensions<'a>(
    text: &str,
    edition: Edition,
    bodies: Bodies,
    extensions: Extensions,
    alloc: Alloc<'a>,
) -> Outline<'a> {
    walk::rows::outline_with_extensions::<SideBySide, ByTokens>(text, parser_edition(edition), bodies, extensions, alloc)
}

/// Scans leading inner attributes (`#![blaze::uniq]`, `#![blaze::records]`,
/// `#![blaze::error_unions]`) at the top of `text` before the first item, returning the combined
/// [`Extensions`] set with zero allocations.
pub fn detect_blaze_extensions(text: &str) -> Extensions {
    let mut rest = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut ext = Extensions::NONE;
    loop {
        // Skip whitespace and comments (`//...`, nested `/*...*/`) using slice prefix stripping
        // (no manual byte indices or hardcoded token lengths).
        loop {
            rest = rest.trim_ascii_start();
            if let Some(after) = rest.strip_prefix("//") {
                rest = match after.split_once('\n') {
                    Some((_, tail)) => tail,
                    None => "",
                };
            } else if let Some(mut after) = rest.strip_prefix("/*") {
                let mut depth = 1usize;
                while depth > 0 && !after.is_empty() {
                    if let Some(tail) = after.strip_prefix("/*") {
                        depth += 1;
                        after = tail;
                    } else if let Some(tail) = after.strip_prefix("*/") {
                        depth -= 1;
                        after = tail;
                    } else {
                        let step = after.chars().next().map_or(1, char::len_utf8);
                        after = after.get(step..).unwrap_or("");
                    }
                }
                rest = after;
            } else {
                break;
            }
        }
        // Only leading `#![...]` inner attributes can enable crate/file extensions.
        let Some(after) = rest.strip_prefix("#![") else {
            break;
        };
        let mut depth = 1usize;
        let mut inner_len = None;
        for (idx, ch) in after.char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        inner_len = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = inner_len else {
            break;
        };
        if let Some(inner) = after.get(..end) {
            let trimmed = inner.trim();
            if let Some(after_blaze) = trimmed.strip_prefix("blaze")
                && let Some(name) = after_blaze.trim_start().strip_prefix("::")
            {
                match name.trim() {
                    "uniq" => ext = ext.with(Extensions::UNIQ),
                    "records" => ext = ext.with(Extensions::RECORDS),
                    "error_unions" => ext = ext.with(Extensions::ERROR_UNIONS),
                    _ => {}
                }
            }
        }
        rest = after.get(end + ']'.len_utf8()..).unwrap_or("");
    }
    ext
}

/// [`outline_with_extensions`] using the blaze syntax extensions declared in `text`'s leading
/// `#![blaze::...]` inner attributes ([`detect_blaze_extensions`]).
pub fn outline_auto_extensions<'a>(text: &str, edition: Edition, bodies: Bodies, alloc: Alloc<'a>) -> Outline<'a> {
    let extensions = detect_blaze_extensions(text);
    outline_with_extensions(text, edition, bodies, extensions, alloc)
}

/// [`outline_with`], its digests hashed by `C`: the seam [`SideBySide`] and its twin
/// [`OneByOne`] are tested equal at.
pub fn outline_hashed_by<'a, C: Compress>(text: &str, edition: Edition, bodies: Bodies, alloc: Alloc<'a>) -> Outline<'a> {
    walk::rows::outline::<C, ByTokens>(text, parser_edition(edition), bodies, alloc)
}

pub use digest::{ByBytes, ByTokens, Compress, Feed, OneByOne, SideBySide, part_digests};

pub use split::{
    Assembled, PART_DOMAIN, Part, Resplit, Speculated, chunk_starts, common_prefix, common_suffix, join, part_digest,
    resplit, speculate, split_top_level,
};

/// Where a range of an item's text is as its tokens count it (`parser::token_key::TokenPlace`):
/// `range` of `item`, the item's text from its first token, read in `edition`. What a record
/// keyed by the item's tokens holds in place of bytes (rule 71): the key stands across a trivia
/// edit, and so does this place, where a byte range would point at stale text. Refused, never
/// clamped (rule 3): a range that starts or ends in trivia, or cuts a token it does not lie in.
pub fn token_place(item: &str, edition: Edition, range: core::ops::Range<usize>) -> Result<parser::token_key::TokenPlace, parser::token_key::PlaceError> {
    parser::token_key::TokenPlace::from_bytes(item, parser_edition(edition), range)
}

/// The bytes of `item` (an item's text today, from its first token, in `edition`) that `place`
/// ([`token_place`]) covers: where a finding stored as tokens is printed.
pub fn place_bytes(place: parser::token_key::TokenPlace, item: &str, edition: Edition) -> Result<core::ops::Range<usize>, parser::token_key::PlaceError> {
    place.to_bytes(item, parser_edition(edition))
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

/// What [`outline`] gives: plain data in the allocator `'a` names, the working form of a file's
/// `FileItems` record ([`Outline::to_file_items`]).
pub struct Outline<'a> {
    items: AVec<'a, Item>,
    attrs: AVec<'a, Attr>,
    file_attrs: AttrSpan,
    errors: AVec<'a, SyntaxError>,
    /// The errors' messages, one after another, each copied from a `&str`.
    messages: AVec<'a, u8>,
    /// Each top-level entry and its sibling key (its parent, kind and name or head), in order:
    /// what its disambiguator counts.
    keys: AVec<'a, (ItemIndex, ContentId)>,
    /// Each entry's names digest, beside `items` (crate::names_digest): what [`Outline::surface`]
    /// hashes. A column of the outline, not a field of blaze-surface's `Item`, so the record's
    /// form is unchanged; position-free, so a part's are the whole file's.
    names: AVec<'a, ContentId>,
    /// The digest of the file's inner attributes but its doc comments, by their text
    /// (crate::names_digest::file_names_digest): what the name solver reads of them, hashed
    /// first by [`Outline::surface`] (names-attr-4, rule 21).
    file_names: ContentId,
    /// The flags of the file's own tokens, outside every entry.
    own_flags: ItemFlags,
    /// `own_flags` and every entry's together.
    flags: ItemFlags,
    /// The blaze syntax extensions active when this outline was parsed.
    extensions: Extensions,
}

impl Outline<'_> {
    /// Copy every column at its exact length into the caller's allocator. The names resolver
    /// uses this to move a file worker's result into the owning crate's frame.
    pub fn copy_in<'b>(&self, alloc: Alloc<'b>) -> Outline<'b> {
        fn exact<'b, T: Copy>(from: &[T], alloc: Alloc<'b>) -> AVec<'b, T> {
            let mut to = AVec::with_capacity_in(from.len(), alloc);
            to.extend_from_slice(from);
            to
        }
        Outline {
            items: exact(&self.items, alloc),
            attrs: exact(&self.attrs, alloc),
            file_attrs: self.file_attrs,
            errors: exact(&self.errors, alloc),
            messages: exact(&self.messages, alloc),
            keys: exact(&self.keys, alloc),
            names: exact(&self.names, alloc),
            file_names: self.file_names,
            own_flags: self.own_flags,
            flags: self.flags,
            extensions: self.extensions,
        }
    }

    /// The blaze syntax extensions active when this outline was parsed.
    pub fn extensions(&self) -> Extensions {
        self.extensions
    }

    /// Every flag the file's tokens set, its own and its entries' ([`ItemFlags`]).
    pub fn flags(&self) -> ItemFlags {
        self.flags
    }

    /// The flags of the file's own tokens, outside every entry (`#![..]`).
    pub fn own_flags(&self) -> ItemFlags {
        self.own_flags
    }

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
            file_flags: self.own_flags,
        };
        parts.write(alloc)
    }

    /// Every entry, in source order: an entry comes before the entries written inside it.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The file's surface: what name resolution reads of it, never a body or a type. For each
    /// entry outside a function's body, in order, its kind, its parent's place and its names
    /// digest ([`Outline::names_digest`]: name, visibility, attributes, a struct's or variant's
    /// form, a `use` tree, an impl's header; the whole entry under a derive or an attribute
    /// macro, and a macro's or macro call's tokens). A body edit, and an edit to a type, a bound
    /// or a const's value, leaves it equal (the drive's `Surface` part; blaze-world's `names
    /// --records`). First the file's inner attributes but its doc comments, by their text
    /// (`file_names`): the root's `#![register_tool]`, `#![no_std]`, `#![no_core]`, a module's
    /// `#![cfg]`, which every module's lowering or the crate's prelude reads.
    pub fn surface(&self) -> ContentId {
        let items = &self.items;
        let mut hasher = blaze_ids::Hasher::new(SURFACE_DOMAIN);
        hasher.id(&self.file_names);
        if self.extensions != Extensions::NONE {
            hasher.part(&[self.extensions.bits()]);
        }
        let mut at = 0;
        while at < items.len() {
            let item = &items[at];
            let parent = item.parent.map_or(u32::MAX, |p| p.0);
            hasher.part(&[item.kind() as u8]).part(&parent.to_le_bytes()).id(&self.names[at]);
            // Everything written inside a function's body is hidden: skip past its last entry.
            at = if item.kind() == ItemKind::Function { item.end.at().max(at + 1) } else { at + 1 };
        }
        hasher.finish()
    }

    /// The names digest of the entry at `at`, in [`NAMES_DOMAIN`]: what name resolution can
    /// observe of it without its types (crate::names_digest, which lists what the solver reads
    /// and what of it is digested).
    pub fn names_digest(&self, at: ItemIndex) -> ContentId {
        self.names[at.at()]
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
            && self.names[..] == other.names[..]
            && self.file_names == other.file_names
            && self.own_flags == other.own_flags
            && self.flags == other.flags
            && self.extensions == other.extensions
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

#[cfg(test)]
mod extensions_tests {
    use super::*;

    #[test]
    fn detect_and_outline_uniq_and_records_extensions() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let src = "//! UI module\n#![blaze::uniq]\n#![blaze::records]\nfn obj_row(ui: &uniq UiWorld, p: { name: &str, depth: u32 = 0, open: bool = false }) {}\n";
        let detected = detect_blaze_extensions(src);
        assert_eq!(detected, Extensions::UNIQ.with(Extensions::RECORDS));

        let out = outline_auto_extensions(src, Edition::E2024, Bodies::Skipped, alloc);
        assert_eq!(out.extensions(), Extensions::UNIQ.with(Extensions::RECORDS));
        assert!(out.errors().is_empty(), "expected zero syntax errors with auto extensions, got {:?}", out.errors());
        assert_eq!(out.items().len(), 1);
        assert_eq!(out.items()[0].kind(), ItemKind::Function);
        assert_eq!(out.items()[0].name.map(|n| out.items()[0].text(src, n)), Some("obj_row"));

        // Surface digest is sensitive to active syntax extensions even without inner attributes.
        let plain = "fn f(x: &u32) {}\n";
        let s_none = outline_with_extensions(plain, Edition::E2024, Bodies::Skipped, Extensions::NONE, alloc).surface();
        let s_uniq = outline_with_extensions(plain, Edition::E2024, Bodies::Skipped, Extensions::UNIQ, alloc).surface();
        assert_ne!(s_none, s_uniq, "extensions must participate in Outline::surface()");
    }
}
