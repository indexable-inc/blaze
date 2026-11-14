//! The plain rows of a `FileImage` and its table root. Every row is a `blaze_format` record of
//! plain fields, so a table of them opens in O(1) and is read in place.

#![blaze::sans_io]
// `blaze_format::record!` writes the `unsafe impl Record` of each row; nothing else here is
// unsafe.
#![allow(unsafe_code)]

use blaze_format::Slice;
use blaze_ids::ContentId;

/// No row: a `parent` at the top, a table index not given.
pub const NONE: u32 = u32::MAX;

/// The layout of a file image's tables, [`ImageTables::layout`]. 2: the read-addressed image
/// (D39): no name pool, a row's header and own-text digests and its escapes; 1 was the
/// text-closure image, whose rows a reader of this layout refuses by name.
pub const LAYOUT: u32 = 2;

blaze_format::record! {
    /// A run of a pool: `len` elements from element `at`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Span: 1 {
        pub at: u32,
        pub len: u32,
    }
}

impl Span {
    pub const EMPTY: Span = Span { at: 0, len: 0 };

    pub fn range(self) -> core::ops::Range<usize> {
        self.at as usize..self.at as usize + self.len as usize
    }
}

/// [`Tok::flags`]: a punctuation token the next punctuation token follows with nothing between
/// (`--` against `- -`, `>>` against `> >`): what a macro's matcher and a proc macro's
/// `Spacing` see. Whitespace anywhere else is in no digest.
pub const JOINT: u8 = 1;
/// [`Tok::flags`]: the token is inside a `macro_rules!` or `macro` definition, so a call or a late
/// read spelled there is not one.
pub const IN_DEFINITION: u8 = 2;

blaze_format::record! {
    /// One non-trivia token: its place in the file's text, its `SyntaxKind`, its flags. Doc
    /// comments are tokens (they are attributes); plain comments and whitespace are not.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Tok: 1 {
        pub start: u32,
        pub len: u32,
        pub kind: u16,
        pub flags: u8,
        pub pad: u8,
    }
}

/// [`ItemRow::flags`]: the row is an item of the unit's module tree (a module-level item, in
/// the file or in an inline module), which has a piece of the surface; else it is an
/// associated item with a body, of the node at `parent`.
pub const NODE: u32 = 1;
/// The row has a body that is keyed (`text`, `body_tokens`).
pub const BODY: u32 = 1 << 1;
/// The row's body holds a macro that is not quiet: a call not on [`crate::quiet::QUIET`], or
/// an item under a derive or an attribute macro. Such a body may make items no text shows; they
/// reach the surface as a fact that body's items task makes, and this flag is in the piece.
pub const CALLS: u32 = 1 << 2;

/// Item kinds, the byte of a stable id ([`blaze_keyed::record::StableId::of`]).
pub mod kind {
    pub const FN: u32 = 1;
    pub const CONST: u32 = 2;
    pub const STATIC: u32 = 3;
    pub const STRUCT: u32 = 4;
    pub const ENUM: u32 = 5;
    pub const UNION: u32 = 6;
    pub const TRAIT: u32 = 7;
    pub const IMPL: u32 = 8;
    pub const ALIAS: u32 = 9;
    pub const MODULE: u32 = 10;
    pub const USE: u32 = 11;
    pub const EXTERN_CRATE: u32 = 12;
    pub const EXTERN_BLOCK: u32 = 13;
    pub const MACRO_CALL: u32 = 14;
    pub const MACRO_RULES: u32 = 15;
    pub const MACRO_DEF: u32 = 16;
    /// The file's inner attributes and inner doc comments, and any token at the file's top level
    /// that is in no item.
    pub const INNER_ATTRS: u32 = 17;
    /// A node at item level that is not an item (global `asm!`, a parse error).
    pub const OTHER: u32 = 18;
}

blaze_format::record! {
    /// One item of a file: a node ([`NODE`]) or an associated item with a body. Rows are in the
    /// file's preorder, and a node's associated rows follow it at once: a row's `parent` is
    /// before it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct ItemRow: 1 {
        /// A node's header: the digest of its tokens with its bodies cut (its own, its
        /// associated items') and its inline module's items cut; an item under an attribute
        /// macro, whose expansion reads all of it, cuts nothing. Zero for an associated row (its
        /// node's header holds its header).
        pub header: ContentId,
        /// The item's own text: the digest of all its tokens, header and body
        /// (`BodyInputs::text`). Zero without [`BODY`].
        pub text: ContentId,
        /// The digests of the items written in the row's body that escape it (the escape
        /// pool), in token order.
        pub escapes: Span,
        /// The in-file path (the string pool): `a::b`, `{impl ...}::f`, with `#n` for a repeat.
        pub path: Span,
        /// The item's own `#[cfg]`, encoded ([`crate::cfgs`]); empty for none.
        pub cfg: Span,
        /// The late reads spelled in the row's header and body (the late pool), in token order.
        pub lates: Span,
        /// The item's tokens and its body's (the token table).
        pub tokens: Span,
        pub body_tokens: Span,
        /// The enclosing inline module, impl, trait or extern block row, or [`NONE`].
        pub parent: u32,
        /// [`kind`].
        pub kind: u32,
        pub flags: u32,
        pub pad: u32,
    }
}

blaze_format::record! {
    /// One late read spelled in the file: which macro (`blaze_keyed::kind::Late` as a byte), its
    /// argument (the string pool), whether that is the unescaped literal or, when it is not a
    /// literal, the argument's token text for the caller to evaluate.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct LateRow: 1 {
        pub name: Span,
        /// The macro's name token (the token table).
        pub token: u32,
        pub late: u8,
        pub literal: u8,
        pub pad: u16,
    }
}

/// [`ModRow::flags`]: an inline module (`mod a { .. }`), which only gives its children a
/// directory.
pub const INLINE: u32 = 1;

blaze_format::record! {
    /// One module declaration: `mod name;` (a file the caller reads) or `mod name { .. }`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct ModRow: 1 {
        /// The module's name (the string pool), `r#` cut.
        pub name: Span,
        /// Its `#[path]`s, each with the `cfg_attr` condition it sits under (the path table).
        pub paths: Span,
        /// The item row of the declaration (its `#[cfg]`).
        pub row: u32,
        /// The enclosing inline module's mod row, or [`NONE`].
        pub parent: u32,
        pub flags: u32,
        pub pad: u32,
    }
}

blaze_format::record! {
    /// One `#[path = ".."]` of a module: the path (the string pool) and its condition (the cfg
    /// pool; empty when unconditional).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct PathRow: 1 {
        pub path: Span,
        pub cond: Span,
    }
}

/// Section ids of a `FileImage`'s root.
pub mod section {
    pub const TABLES: u32 = 0;
    pub const TOKENS: u32 = 1;
    pub const LINES: u32 = 2;
    pub const ITEMS: u32 = 3;
    pub const ESCAPES: u32 = 4;
    pub const STRS: u32 = 5;
    pub const CFGS: u32 = 6;
    pub const LATES: u32 = 7;
    pub const MODS: u32 = 8;
    pub const PATHS: u32 = 9;
    pub const TREE: u32 = 10;
}

blaze_format::record! {
    /// The tables of a `FileImage`, the record at its [`section::TABLES`] section.
    #[derive(Clone, Copy, Debug)]
    pub struct ImageTables: 1 {
        pub tokens: Slice<Tok>,
        pub lines: Slice<u32>,
        pub items: Slice<ItemRow>,
        pub escapes: Slice<ContentId>,
        pub strs: Slice<u8>,
        pub cfgs: Slice<u8>,
        pub lates: Slice<LateRow>,
        pub mods: Slice<ModRow>,
        pub paths: Slice<PathRow>,
        /// The parse record (`syntax::image::write`), at a multiple of 16 of the buffer.
        pub tree: Slice<u8>,
        /// The file's inner `#![cfg]`, encoded; empty for none.
        pub file_cfg: Span,
        /// The reading word the file was parsed under ([`crate::image::Reading::word`]).
        pub reading: u32,
        /// [`LAYOUT`].
        pub layout: u32,
    }
}
