//! `Task::Key` of the keyed driver (docs/v2/plan/design/host.md, "blaze-unitkey"; D39,
//! docs/v2/plan/design/read-addressed.md): every key of a unit (its manifest's, its surface's,
//! each body's) from its own text alone, before any work. Reads no file, no store, no clock and
//! nothing of any other unit: the caller hands in bytes and images and files what comes back.
//!
//! A key says what a record is, never what its text can reach: a record's reads are its shape,
//! asked and answered by its task, so nothing here follows a name.
//!
//! # The record
//!
//! **`FileImage`** ([`image::image`], read by [`read::Image`]), keyed by
//! `FileInputs { tool, edition: the reading word, bytes }`, root `Sections`, plain. Its tables:
//! the non-trivia tokens (start, length, kind, flags; the text is the caller's file, never
//! copied), the line starts, the parse record (`syntax::image`, 16-aligned), one row per item
//! ([`rows::ItemRow`]: its header and own-text digests, its escapes, path, `#[cfg]`, late reads,
//! token spans, flags), the escape pool, a string pool, the encoded `#[cfg]` predicates, the
//! late-read asks, and the module declarations with their `#[path]`s. Every table is a plain
//! array read in place: opening one checks bounds only, nothing is decoded to the heap.
//!
//! # How a file is named, outside the unit's root too
//!
//! A file outside the directory of the unit's root (core's `#[path = "../../stdarch/..."]`
//! modules, std's `include!("../../core/src/...")`) is named exactly as one inside it, and
//! nothing of the machine names any file (rule 25): its image holds no path (its key is the
//! toolchain, the reading and the bytes); the manifest holds its image key at its place in the
//! unit's file order (the root, then breadth first in declaration order, `mods::mod_files`) and
//! no path; its items are named by its module path (`a::b`, the path of the `mod`s that reach
//! it, `#[path]` or not; a later file under a module path an earlier one has, as two arms of
//! one macro call give, is `a::b{k}`, `mods::module_twice`) in every stable id; a late read is
//! named by its argument as written.
//! Where a keyed record must name a file by path (a crate's pages), the path is the file's from
//! the crate root's directory, `/`-separated, lexically normal ([`mods::normal`]: no `.`, no
//! empty part, a `..` only leading, one for each step up):
//! `../../stdarch/crates/core_arch/src/mod.rs` from core's `src/lib.rs`. The gather names every
//! file it reads by its normal path, so one file reached by two spellings is one file. An
//! absolute path is only ever read or printed, never kept.
//!
//! # Rules
//!
//! - A node is an item of the module tree (in a file or an inline module); its associated
//!   items with a body are rows of their own, after it. Its header is its tokens with its bodies
//!   cut and its inline module's items cut; an item under an attribute macro cuts nothing.
//! - A body's key is its item's own tokens, header and body, with the unit and its stable id.
//! - An item written in a body that escapes it (an impl, a `#[macro_export]` macro, an item with
//!   `#[no_mangle]`, `#[export_name]`, `#[used]` or `#[link_section]`, a `#[link]` block) joins
//!   the enclosing node's piece by its header digest.
//! - A body holding a macro that is not quiet ([`quiet::QUIET`]), a derive or an attribute macro
//!   sets one flag in its node's piece: what that makes is a fact its task makes.
//! - What a `const fn`, an `async fn`, a return-position `impl Trait` or a `#[define_opaque]`
//!   body shows others is a fact its body's task makes, never a key rule.
//! - Tokens are hashed as (kind, joint, length-prefixed text); comments and whitespace are in no
//!   digest but the file image's key.
//!
//! # Functions, floors and costs
//!
//! - [`image::key`]: one hash of the file's bytes. The floor: an unchanged file costs this and
//!   one store lookup, never a parse.
//! - [`image::image`]: one parse, one walk of the tree and at most three passes over each token:
//!   O(file). Floor: the one reparse of the edited file.
//! - [`mods::mod_files`]: the files a file's `mod` declarations name, O(declarations).
//! - [`gather::gather`]: a unit's files and late answers from its root, through the caller's
//!   source: one read and one image per file (an image once per content), the one gather.
//! - [`unit::asks`]: the late reads of a unit's files, O(rows + late reads).
//! - [`late::reads`] and [`late::var`]: which file or variable a late read reads, the one rule
//!   the command and the tests answer by; O(1) per read and one binary search per variable.
//! - [`unit::key`]: every key, one pass over the unit's item rows, no token read: O(rows log
//!   rows). With the edited file's image, that is the floor of keying after an edit.
//! - [`read::Image::find`] and [`read::Image::syntax`]: an item's tokens and bytes from its
//!   stable id, for the body task, in place.
//!
//! So a comment edit is one reparse, and the file image's and the manifest's keys move; a body
//! edit moves those and one body key; a header edit moves those, one body key and the surface
//! key.

#![deny(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
// `#![blaze::sans_io]` in each module; inert under rustc.
#![register_tool(blaze)]

pub mod cfgs;
pub mod gather;
pub mod image;
pub mod late;
pub mod mods;
pub mod quiet;
pub mod read;
pub mod rows;
pub mod unit;

#[cfg(test)]
mod tests;

use core::fmt;

use blaze_keyed::record::{RecordError, StableId};

/// Why a file has no image: each names the file's item and the rule that refused it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImageError {
    /// The file is not UTF-8: Rust source is.
    NotText,
    /// A file of 4 GiB or more, or a table past `u32` rows.
    TooLarge { what: &'static str },
    /// A `#[cfg]` predicate that does not parse, on the item at `item` (its in-file path).
    Cfg { item: Vec<u8>, rule: &'static str },
    /// A reading word whose edition or syntax extensions blaze does not know.
    Reading { word: u16 },
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::NotText => write!(f, "a source file that is not UTF-8"),
            ImageError::TooLarge { what } => write!(f, "{what} past 2^32"),
            ImageError::Cfg { item, rule } => {
                write!(f, "`{}`: {rule}", String::from_utf8_lossy(item))
            }
            ImageError::Reading { word } => {
                write!(f, "reading word {word:#06x} names no edition and extensions blaze knows")
            }
        }
    }
}

/// An item of a unit by its place: file `file` of the unit's file order, item row `row` of that
/// file's image. Item-relative: the caller turns it into `file:line` with the image's line table
/// (`read::Image::lines`) when it prints it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ItemAt {
    pub file: u32,
    pub row: u32,
}

/// Why a unit has no keys.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeyError {
    /// File `file`'s image does not open under its key.
    Image { file: u32, error: RecordError },
    /// A late read of file `file` the caller gave no answer for.
    Unanswered { file: u32, late: u8, name: Vec<u8> },
    /// Answers not sorted by (file, late, name), or one given twice.
    Answers,
    /// File `file`'s image has a span outside its pool, a `#[cfg]` that does not decode or a
    /// row out of its order: not an image this crate wrote.
    Pool { file: u32, error: read::OutOfPool },
    /// Two active items of the unit with one stable id: one key would name two values (rule 21).
    /// Within a file the image's rule makes this impossible (`image`, "Stable ids"); across files
    /// it is two files given one module path, which no program rustc accepts has (E0428), and
    /// which the gather never gives (two arms' files are suffixed, `mods::gathered_module`). Both
    /// items by their place, so the caller prints each at its file and line; `path` is the
    /// first's `module::path`.
    TwoItems { stable: StableId, path: Vec<u8>, first: ItemAt, second: ItemAt },
    /// More rows than a table holds.
    TooLarge { what: &'static str },
    /// `Unit::names` not one per file of `Unit::files`: the manifest's key would miss a file's
    /// printed name, or hold one of no file (rule 21).
    Names { files: usize, names: usize },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Image { file, error } => write!(f, "file {file}'s image: {error}"),
            KeyError::Unanswered { file, late, name } => write!(
                f,
                "file {file}: late read {late} of `{}` has no answer",
                String::from_utf8_lossy(name)
            ),
            KeyError::Answers => write!(f, "late answers not sorted by (file, late, name), each once"),
            KeyError::Pool { file, error } => {
                write!(f, "file {file}'s image: {} at {}", error.pool, error.at)
            }
            KeyError::TwoItems { stable, path, first, second } => write!(
                f,
                "two items `{}` of one stable id {stable:?}, row {} of file {} and row {} of file {}: an item's stable id is its own",
                String::from_utf8_lossy(path),
                first.row,
                first.file,
                second.row,
                second.file
            ),
            KeyError::TooLarge { what } => write!(f, "{what} past 2^32"),
            KeyError::Names { files, names } => write!(
                f,
                "{names} printed names for {files} files: the manifest's key takes one per file (rule 21, keys-cover-every-input)"
            ),
        }
    }
}
