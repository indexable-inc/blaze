//! A file image read in place. Opening one checks its head (kind, version, key), its tables'
//! layout and every table's bounds and alignment once; every accessor after that is a slice
//! index.
//!
//! What a body task gets from the image to find its item's syntax without parsing the file
//! again: [`Image::find`] (a stable id to the item's row) and [`Image::syntax`] (the row's
//! tokens and its body's, and their byte ranges in the file's text, all read in place), with
//! [`Image::tree`], the file's parse record, which `syntax::image::read` maps back over the
//! file's text; the item's node is the one the byte range covers.
//!
//! Cost of [`Image::open`]: O(sections), plus the bounds check of each table (O(1) per table:
//! rows are plain records). Floor: the same; nothing is decoded.

#![blaze::sans_io]

use core::ops::Range;

use blaze_format::record_at;
use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{FileImage, Key},
    record::{self, RecordError, Section, StableId},
};
use cfg::CfgOptions;

use crate::{
    cfgs,
    rows::{BODY, ImageTables, ItemRow, LAYOUT, LateRow, ModRow, NONE, PathRow, Span, Tok, section},
};

/// The section `id` of a `Sections` record's table, or a refusal naming it.
fn section_at(sections: &[Section], id: u32) -> Result<&Section, RecordError> {
    sections.iter().find(|section| section.id == id).ok_or(RecordError::Format(
        blaze_format::FormatError::Invalid { at: 0, what: "a record without its tables section" },
    ))
}

/// A file image, open.
#[derive(Clone, Copy)]
pub struct Image<'b> {
    tables: &'b ImageTables,
    tokens: &'b [Tok],
    lines: &'b [u32],
    items: &'b [ItemRow],
    escapes: &'b [ContentId],
    strs: &'b [u8],
    cfgs: &'b [u8],
    lates: &'b [LateRow],
    mods: &'b [ModRow],
    paths: &'b [PathRow],
    tree: &'b [u8],
}

/// One item's syntax, read in place: what a body task needs to find the item in the file's
/// parse ([`Image::tree`]) without parsing anything.
#[derive(Clone, Copy, Debug)]
pub struct ItemSyntax<'b> {
    pub row: &'b ItemRow,
    /// The item's tokens (attributes, header and body), and their bytes in the file's text.
    pub tokens: &'b [Tok],
    pub range: (u32, u32),
    /// The body's tokens and bytes, when the item has a body.
    pub body: Option<(&'b [Tok], (u32, u32))>,
}

impl ItemSyntax<'_> {
    /// The item's bytes in the file's text, `start..end`.
    pub fn bytes(&self) -> Range<usize> {
        self.range.0 as usize..self.range.1 as usize
    }
}

/// The bytes `tokens` cover: the first's start to the last's end.
fn covered(tokens: &[Tok]) -> (u32, u32) {
    match (tokens.first(), tokens.last()) {
        (Some(first), Some(last)) => (first.start, last.start + last.len),
        _ => (0, 0),
    }
}

impl<'b> Image<'b> {
    /// Opens `buffer` (16-aligned) as the file image of `key`. An image of another layout (one a
    /// former blaze wrote) is refused by name, never read as this one.
    pub fn open(buffer: &'b [u8], key: &Key<FileImage>) -> Result<Image<'b>, RecordError> {
        let root = record::open::<FileImage>(buffer, key)?;
        let sections = root.sections.follow(buffer).map_err(RecordError::Format)?;
        let at = section_at(sections, section::TABLES)?.at;
        let tables = record_at::<ImageTables>(buffer, at).map_err(RecordError::Format)?;
        if tables.layout != LAYOUT {
            return Err(RecordError::Format(blaze_format::FormatError::Invalid {
                at,
                what: "a file image of another layout than blaze-unitkey's `rows::LAYOUT`",
            }));
        }
        let format = RecordError::Format;
        Ok(Image {
            tables,
            tokens: tables.tokens.follow(buffer).map_err(format)?,
            lines: tables.lines.follow(buffer).map_err(format)?,
            items: tables.items.follow(buffer).map_err(format)?,
            escapes: tables.escapes.follow(buffer).map_err(format)?,
            strs: tables.strs.follow(buffer).map_err(format)?,
            cfgs: tables.cfgs.follow(buffer).map_err(format)?,
            lates: tables.lates.follow(buffer).map_err(format)?,
            mods: tables.mods.follow(buffer).map_err(format)?,
            paths: tables.paths.follow(buffer).map_err(format)?,
            tree: tables.tree.follow(buffer).map_err(format)?,
        })
    }

    pub fn tokens(&self) -> &'b [Tok] {
        self.tokens
    }

    /// The byte offset of each line's start, the first 0.
    pub fn lines(&self) -> &'b [u32] {
        self.lines
    }

    pub fn items(&self) -> &'b [ItemRow] {
        self.items
    }

    pub fn mods(&self) -> &'b [ModRow] {
        self.mods
    }

    /// The parse record, for `syntax::image::read` with the file's text.
    pub fn tree(&self) -> &'b [u8] {
        self.tree
    }

    /// The reading word the file was parsed under.
    pub fn reading(&self) -> u16 {
        self.tables.reading as u16
    }

    /// A run of the escape pool: the digests of what escapes a row's body. A span outside its
    /// pool is refused ([`OutOfPool`]): an image this crate did not write.
    pub fn escapes(&self, span: Span) -> Result<&'b [ContentId], OutOfPool> {
        run(self.escapes, span, "the escape pool")
    }

    pub fn str(&self, span: Span) -> Result<&'b [u8], OutOfPool> {
        run(self.strs, span, "the string pool")
    }

    pub fn cfg(&self, span: Span) -> Result<&'b [u8], OutOfPool> {
        run(self.cfgs, span, "the cfg pool")
    }

    pub fn lates(&self, span: Span) -> Result<&'b [LateRow], OutOfPool> {
        run(self.lates, span, "the late pool")
    }

    pub fn paths(&self, span: Span) -> Result<&'b [PathRow], OutOfPool> {
        run(self.paths, span, "the path table")
    }

    /// The file's inner `#![cfg]`, encoded.
    pub fn file_cfg(&self) -> Result<&'b [u8], OutOfPool> {
        self.cfg(self.tables.file_cfg)
    }

    /// Whether row `row` is active under `options`: the file's `#![cfg]`, its own `#[cfg]` and
    /// every enclosing row's hold. Cost: O(depth) predicate walks. (Keying decides every row at
    /// once, in one pass: `unit::active`.)
    pub fn active(&self, row: u32, options: &CfgOptions) -> Result<bool, OutOfPool> {
        let undecodable = |_| OutOfPool { pool: "a `#[cfg]` that does not decode", at: row };
        if !cfgs::holds(self.file_cfg()?, options).map_err(undecodable)? {
            return Ok(false);
        }
        let mut at = row;
        while at != NONE {
            let item = self.items.get(at as usize).ok_or(OutOfPool { pool: "the item table", at })?;
            // Rows are in preorder, each parent before its child: the chain ends.
            if item.parent != NONE && item.parent >= at {
                return Err(OutOfPool { pool: "a parent row after its child", at });
            }
            if !cfgs::holds(self.cfg(item.cfg)?, options).map_err(undecodable)? {
                return Ok(false);
            }
            at = item.parent;
        }
        Ok(true)
    }

    /// Row `row`'s stable id in a file at module path `module` (`` for the crate root, `a::b`):
    /// `module::path` and the row's kind. One hash of the path.
    pub fn stable_id(&self, module: &[u8], row: u32) -> Result<StableId, OutOfPool> {
        let item = self.items.get(row as usize).ok_or(OutOfPool { pool: "the item table", at: row })?;
        let path = self.str(item.path)?;
        Ok(stable_of(module, path, item.kind))
    }

    /// The row of the item `stable` in a file at module path `module`, or `None` when the file
    /// has no such item. Cost: one hash of each row's path until it is found; no token is read.
    pub fn find(&self, module: &[u8], stable: StableId) -> Result<Option<u32>, OutOfPool> {
        for at in 0..self.items.len() as u32 {
            if self.stable_id(module, at)? == stable {
                return Ok(Some(at));
            }
        }
        Ok(None)
    }

    /// Row `row`'s syntax, in place: its tokens and its body's, with their bytes in the text.
    pub fn syntax(&self, row: u32) -> Result<ItemSyntax<'b>, OutOfPool> {
        let item = self.items.get(row as usize).ok_or(OutOfPool { pool: "the item table", at: row })?;
        let tokens = run(self.tokens, item.tokens, "the token table")?;
        let body = match item.flags & BODY != 0 {
            true => {
                let body = run(self.tokens, item.body_tokens, "the token table")?;
                Some((body, covered(body)))
            }
            false => None,
        };
        Ok(ItemSyntax { row: item, tokens, range: covered(tokens), body })
    }
}

/// A stable id: `module::path` (or `path` at the crate root) and the kind.
pub(crate) fn stable_of(module: &[u8], path: &[u8], kind: u32) -> StableId {
    // `module::path` without a buffer: `StableId::of` hashes the path as one part, so the bytes
    // are joined first, on the stack when they fit.
    let mut stack = [0u8; 256];
    let len = module.len() + if module.is_empty() { 0 } else { 2 } + path.len();
    match len <= stack.len() {
        true => {
            let mut at = 0;
            for part in [module, if module.is_empty() { &b""[..] } else { &b"::"[..] }, path] {
                stack[at..at + part.len()].copy_from_slice(part);
                at += part.len();
            }
            StableId::of(&stack[..len], kind as u8)
        }
        false => {
            let mut joined = Vec::with_capacity_in(len, std::alloc::Global);
            joined.extend_from_slice(module);
            if !module.is_empty() {
                joined.extend_from_slice(b"::");
            }
            joined.extend_from_slice(path);
            StableId::of(&joined, kind as u8)
        }
    }
}

/// A span of a row that is not inside its pool, or a row chain that does not end: the image is
/// not one this crate wrote. `at`: the span's start, or the row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutOfPool {
    pub pool: &'static str,
    pub at: u32,
}

fn run<'b, T>(pool: &'b [T], span: Span, name: &'static str) -> Result<&'b [T], OutOfPool> {
    pool.get(span.range()).ok_or(OutOfPool { pool: name, at: span.at })
}
