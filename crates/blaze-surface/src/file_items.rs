//! [`FileItems`]: one file's entries, in the file's order, keyed by the file's bytes, its
//! edition and the outline's version: everything blaze-file-items' outline holds, read in place.
//! No tree is kept: a later question re-reads an entry's bytes by its ranges.
//!
//! The value types are defined here once, for the outline that makes them and the record that
//! keeps them (rule 30): [`Item`] and its [`ItemDetail`], [`Attr`], [`AttrKind`], [`AttrSpan`],
//! [`Rename`], [`SyntaxError`] and [`ErrorOrigin`]. An outline is written with
//! [`FileItems::write`] and read back with [`FileItemsView`], which answers what the outline
//! answers: `items`, `item`, `children`, `attrs`, `file_attrs`, `errors` and `message`.
//!
//! # Positions
//!
//! An entry's own [`Item::range`] is where it is in the file, and is what slicing the file's
//! text needs: the record is keyed by the file's bytes, so it moves exactly when the key does.
//! Every other range of an entry (its name, signature, body, attributes, the ranges of its
//! [`ItemDetail`] and its visibility's path) is counted from the entry's first byte (rule 71).
//! The file's own inner attributes are counted from the file's start, and a [`SyntaxError`]'s
//! range is the file's.
//!
//! # Bytes
//!
//! A value has one encoding: every slot a variant does not use is zero, and [`FileItemsView::read`]
//! decodes each row and refuses it unless encoding the value again gives its bytes, so two
//! writers of one outline write equal bytes and a record holds no bytes its values do not say
//! (rule 25).

use core::fmt;
use core::mem::offset_of;
use core::ops::Deref;

use blaze_format::{FormatError, Slice, Writer, WriterOpts};
use blaze_id::{ContentId, Portable};
use blaze_own::{Alloc, Vec};

use crate::column::{
    Column, RecordBytes, Strings, Text, at_of, at_of_record, check_text, count, finish_with,
    index_of, index_word, text_of, wide,
};
use crate::error::BuildError;
// `byte_enum!` names `fmt`, `Portable`, `FormatError` and `at_of` where it is called.
use crate::kind::{ItemIndex, ItemKind, Range, WrittenVis, WrittenVisTag, byte_enum};
use crate::path::Rename;

/// What an entry is, and what each kind has that the others have not. Every range is relative
/// to the entry's start. Its kind alone, the byte records and keys hold, is [`ItemDetail::kind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemDetail {
    /// `fn`. `body_holds_items`: its body holds an item or a macro call, at any depth.
    Fn { body_holds_items: bool },
    Struct,
    Enum,
    Union,
    Trait,
    /// `trait A = B + C;`.
    TraitAlias,
    Impl,
    /// `const`. `body_holds_items` as for [`ItemDetail::Fn`], of its value.
    Const { body_holds_items: bool },
    /// `static`. `body_holds_items` as for [`ItemDetail::Fn`], of its value.
    Static { body_holds_items: bool },
    TypeAlias,
    /// `mod m { .. }` (`inline`) or `mod m;`. `path`: the string literal of its outer
    /// `#[path = ".."]`, quotes and escapes as written.
    Mod { inline: bool, path: Option<Range> },
    /// `use`: the use tree (no `use`, no `;`).
    Use { tree: Option<Range> },
    /// `extern crate name as rename;`; the name is [`Item::name`], and the rename's name a
    /// range of the item.
    ExternCrate { rename: Rename<Range> },
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
    /// An item-level stretch the parser could not read as an item (its message is in the
    /// errors): a stray token, an unmatched `}`, an attribute with no item, an inner attribute
    /// after the first item.
    Error,
    /// An item-level node of a kind the outline does not take apart (`builtin # global_asm(..)`
    /// is the one the grammar has): listed with its syntax kind's number in the parser of the
    /// outline's version (blaze-file-items turns it back into the parser's `SyntaxKind`), never
    /// dropped.
    Unhandled { syntax: u16 },
}

/// The slots of an [`ItemDetail`] in a row: two ranges, one flag and a syntax kind, each zero
/// where the variant has none.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Payload {
    first: Option<Range>,
    second: Option<Range>,
    flag: bool,
    syntax: u16,
}

impl ItemDetail {
    /// Its kind: the variant without its data.
    pub const fn kind(&self) -> ItemKind {
        match self {
            ItemDetail::Fn { .. } => ItemKind::Function,
            ItemDetail::Struct => ItemKind::Struct,
            ItemDetail::Enum => ItemKind::Enum,
            ItemDetail::Union => ItemKind::Union,
            ItemDetail::Trait => ItemKind::Trait,
            ItemDetail::TraitAlias => ItemKind::TraitAlias,
            ItemDetail::Impl => ItemKind::Impl,
            ItemDetail::Const { .. } => ItemKind::Const,
            ItemDetail::Static { .. } => ItemKind::Static,
            ItemDetail::TypeAlias => ItemKind::TypeAlias,
            ItemDetail::Mod { .. } => ItemKind::Module,
            ItemDetail::Use { .. } => ItemKind::Use,
            ItemDetail::ExternCrate { .. } => ItemKind::ExternCrate,
            ItemDetail::ExternBlock { .. } => ItemKind::ExternBlock,
            ItemDetail::MacroRules { .. } => ItemKind::MacroRules,
            ItemDetail::MacroDef => ItemKind::Macro2,
            ItemDetail::MacroCall { .. } => ItemKind::MacroCall,
            ItemDetail::Variant { .. } => ItemKind::Variant,
            ItemDetail::Field { .. } => ItemKind::Field,
            ItemDetail::Error => ItemKind::Error,
            ItemDetail::Unhandled { .. } => ItemKind::Unhandled,
        }
    }

    /// The ranges it holds, each relative to its entry's start.
    fn ranges(&self) -> [Option<Range>; 2] {
        let payload = self.split().1;
        [payload.first, payload.second]
    }

    fn split(self) -> (ItemKind, Payload) {
        let none = Payload::default();
        let payload = match self {
            ItemDetail::Fn { body_holds_items }
            | ItemDetail::Const { body_holds_items }
            | ItemDetail::Static { body_holds_items } => Payload { flag: body_holds_items, ..none },
            ItemDetail::Mod { inline, path } => Payload { first: path, flag: inline, ..none },
            ItemDetail::Use { tree: first }
            | ItemDetail::ExternBlock { abi: first }
            | ItemDetail::Variant { discriminant: first } => Payload { first, ..none },
            ItemDetail::ExternCrate { rename } => match rename {
                Rename::Same => none,
                Rename::To(name) => Payload { first: Some(name), ..none },
                Rename::Underscore => Payload { flag: true, ..none },
            },
            ItemDetail::MacroRules { exported } => Payload { flag: exported, ..none },
            ItemDetail::MacroCall { path: first, input: second }
            | ItemDetail::Field { ty: first, default: second } => Payload { first, second, ..none },
            ItemDetail::Unhandled { syntax } => Payload { syntax, ..none },
            ItemDetail::Struct
            | ItemDetail::Enum
            | ItemDetail::Union
            | ItemDetail::Trait
            | ItemDetail::TraitAlias
            | ItemDetail::Impl
            | ItemDetail::TypeAlias
            | ItemDetail::MacroDef
            | ItemDetail::Error => none,
        };
        (self.kind(), payload)
    }

    /// What [`ItemDetail::split`] gave. A slot the kind has no use for is not read: the view
    /// refuses a row whose value does not encode back to it.
    fn join(kind: ItemKind, payload: Payload) -> ItemDetail {
        let Payload { first, second, flag, syntax } = payload;
        match kind {
            ItemKind::Function => ItemDetail::Fn { body_holds_items: flag },
            ItemKind::Struct => ItemDetail::Struct,
            ItemKind::Enum => ItemDetail::Enum,
            ItemKind::Union => ItemDetail::Union,
            ItemKind::Trait => ItemDetail::Trait,
            ItemKind::TraitAlias => ItemDetail::TraitAlias,
            ItemKind::Impl => ItemDetail::Impl,
            ItemKind::Const => ItemDetail::Const { body_holds_items: flag },
            ItemKind::Static => ItemDetail::Static { body_holds_items: flag },
            ItemKind::TypeAlias => ItemDetail::TypeAlias,
            ItemKind::Module => ItemDetail::Mod { inline: flag, path: first },
            ItemKind::Use => ItemDetail::Use { tree: first },
            ItemKind::ExternCrate => ItemDetail::ExternCrate {
                rename: match (first, flag) {
                    (Some(name), _) => Rename::To(name),
                    (None, true) => Rename::Underscore,
                    (None, false) => Rename::Same,
                },
            },
            ItemKind::ExternBlock => ItemDetail::ExternBlock { abi: first },
            ItemKind::MacroRules => ItemDetail::MacroRules { exported: flag },
            ItemKind::Macro2 => ItemDetail::MacroDef,
            ItemKind::MacroCall => ItemDetail::MacroCall { path: first, input: second },
            ItemKind::Variant => ItemDetail::Variant { discriminant: first },
            ItemKind::Field => ItemDetail::Field { ty: first, default: second },
            ItemKind::Error => ItemDetail::Error,
            ItemKind::Unhandled => ItemDetail::Unhandled { syntax },
        }
    }
}

blaze_format::record! {
    /// A run of a file's attributes, `start..end` in its list of them.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct AttrSpan: 1 {
        pub start: u32,
        pub end: u32,
    }
}

impl AttrSpan {
    /// As a range of `usize`, for slicing.
    pub fn to_usize(self) -> core::ops::Range<usize> {
        wide(self.start)..wide(self.end)
    }
}

/// What an entry's own tokens are that a program given capabilities by type must not hold
/// (docs/v2/plan/design/run.md, "What a program is given"): each bit is read from the tokens,
/// so the word in a string, a comment or an identifier (`unsafe_code`, `r#unsafe`) is none.
/// An entry's own tokens are those of its range outside its child entries (a `fn` body's
/// nested items are the `fn`'s, as they are no entries); [`FileItemsView::flags`] answers for
/// a file, [`flags_of_files`] for a list of them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ItemFlags(u8);

// SAFETY: a value is its one byte, the same on every host and in every snapshot.
unsafe impl Portable for ItemFlags {}

// SAFETY: `#[repr(Rust)]` around one `u8`, a byte with no padding and no reference. `check`
// accepts exactly the bytes with no bit but the declared ones, so every byte it accepts is a
// value; `PLAIN` stays false, so no byte is read unchecked.
unsafe impl blaze_format::Record for ItemFlags {
    const SCHEMA: blaze_format::SchemaHash = blaze_format::schema_of(&[
        blaze_format::Part::Str("ItemFlags"),
        blaze_format::Part::U64(1),
        blaze_format::Part::Str("UNSAFE"),
        blaze_format::Part::U64(1),
        blaze_format::Part::Str("FOREIGN"),
        blaze_format::Part::U64(1 << 1),
        blaze_format::Part::Str("ASM"),
        blaze_format::Part::U64(1 << 2),
        blaze_format::Part::Str("MACRO_CALL"),
        blaze_format::Part::U64(1 << 3),
    ]);

    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        match bytes {
            [byte] if ItemFlags::from_bits(*byte).is_some() => Ok(()),
            _ => Err(FormatError::Invalid { at: at_of(bytes, buffer), what: "a byte of entry flags no entry has" }),
        }
    }
}

impl ItemFlags {
    pub const NONE: ItemFlags = ItemFlags(0);
    /// The keyword `unsafe`: a block, `fn`, `impl`, `trait`, extern block or `unsafe(..)`
    /// attribute.
    pub const UNSAFE: ItemFlags = ItemFlags(1);
    /// Code that is not Rust's: an extern block, an `extern fn` of another ABI than `"Rust"`
    /// (no ABI is `"C"`), or `#[link]`, `#[no_mangle]`, `#[export_name]`, `#[link_section]`,
    /// `#[linkage]` or `#[naked]`, alone, under `unsafe(..)` or under `cfg_attr`.
    pub const FOREIGN: ItemFlags = ItemFlags(1 << 1);
    /// `asm!`, `global_asm!` or `naked_asm!`, by the macro's last path segment.
    pub const ASM: ItemFlags = ItemFlags(1 << 2);
    /// A macro call whose expansion may add any of the others, unknown until it is expanded: a
    /// bang call (but `asm!` and its kin, [`ItemFlags::ASM`]), a derive, or an attribute that
    /// is no inert built-in one. A later stage that expands it clears this bit.
    pub const MACRO_CALL: ItemFlags = ItemFlags(1 << 3);
    /// Every flag, in bit order, with its name.
    pub const NAMED: [(ItemFlags, &'static str); 4] = [
        (ItemFlags::UNSAFE, "unsafe"),
        (ItemFlags::FOREIGN, "foreign"),
        (ItemFlags::ASM, "asm"),
        (ItemFlags::MACRO_CALL, "macro call"),
    ];

    const ALL: u8 = (1 << 4) - 1;

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The flags of `bits`, if no bit is one no flag has.
    pub const fn from_bits(bits: u8) -> Option<ItemFlags> {
        match bits & !ItemFlags::ALL {
            0 => Some(ItemFlags(bits)),
            _ => None,
        }
    }

    #[must_use]
    pub const fn union(self, other: ItemFlags) -> ItemFlags {
        ItemFlags(self.0 | other.0)
    }

    /// These flags with `other`'s cleared: what a later stage that knows better leaves.
    #[must_use]
    pub const fn without(self, other: ItemFlags) -> ItemFlags {
        ItemFlags(self.0 & !other.0)
    }

    pub const fn contains(self, other: ItemFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for ItemFlags {
    type Output = ItemFlags;

    fn bitor(self, other: ItemFlags) -> ItemFlags {
        self.union(other)
    }
}

impl core::ops::BitOrAssign for ItemFlags {
    fn bitor_assign(&mut self, other: ItemFlags) {
        *self = self.union(other);
    }
}

impl fmt::Debug for ItemFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut set = f.debug_set();
        for (flag, name) in ItemFlags::NAMED {
            if self.contains(flag) {
                set.entry(&format_args!("{name}"));
            }
        }
        set.finish()
    }
}

/// The flags of every file in `files` together: [`ItemFlags::is_empty`] answers "none set".
pub fn flags_of_files<'a, 'f: 'a>(files: impl IntoIterator<Item = &'a FileItemsView<'f>>) -> ItemFlags {
    files.into_iter().fold(ItemFlags::NONE, |flags, file| flags | file.flags())
}

/// Each entry of `files` with a flag set, by the file's position in `files`: which entries set
/// which. A file with none is passed over in one read of its record's root.
pub fn flagged_in_files<'a, 'f: 'a>(
    files: impl IntoIterator<Item = &'a FileItemsView<'f>>,
) -> impl Iterator<Item = (usize, ItemIndex, ItemFlags)> {
    files.into_iter().enumerate().flat_map(|(at, file)| file.flagged().map(move |(index, flags)| (at, index, flags)))
}

/// One entry of a file: an item, a variant, a field, an item-position macro call or a stretch
/// the parser could not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Item {
    pub detail: ItemDetail,
    /// The entry it is written in: a module, trait, impl, extern block, enum, struct, union
    /// or variant. `None` for the file's own items.
    pub parent: Option<ItemIndex>,
    /// One past its last descendant among the file's entries: its descendants are the entries
    /// after it, up to here.
    pub end: ItemIndex,
    /// Where it is in the file: from its first outer attribute or doc comment (or its first
    /// token) to its last token.
    pub range: Range,
    /// Its name, as written (`r#` included); `None` for an impl, a `use`, an extern block, a
    /// macro call, a tuple field, `const _` and an error.
    pub name: Option<Range>,
    /// Its visibility as written, the path of `pub(in path)` relative to the entry's start.
    pub visibility: WrittenVis<Range>,
    /// Everything before its body: from its start (outer attributes included) to its body's
    /// start, or, with no body, to the end of its last token before a closing `;`. For a
    /// `const` or `static`, up to the `=`.
    pub signature: Range,
    /// The function's block, the module's, trait's, impl's or extern block's `{ .. }`, the
    /// enum's variants, the struct's, union's or variant's fields, the macro's or macro
    /// call's token tree, or a `const`'s or `static`'s value.
    pub body: Option<Range>,
    /// Its attributes and doc comments, outer then inner (a module's `#![..]`), in the file's
    /// list of them.
    pub attrs: AttrSpan,
    /// How many earlier siblings have the same kind and the same name (blaze-file-items'
    /// docs, "Identity").
    pub disambiguator: u32,
    /// Its bytes' [`ContentId`] in blaze-file-items' `ITEM_DOMAIN`.
    pub hash: ContentId,
    /// Its bytes before its body, in blaze-file-items' `ITEM_DOMAIN`.
    pub signature_hash: ContentId,
    /// What its own tokens hold that capabilities by type refuse.
    pub flags: ItemFlags,
}

impl Item {
    /// What it is.
    pub const fn kind(&self) -> ItemKind {
        self.detail.kind()
    }

    /// `range`, one of this entry's entry-relative ranges, as a range of the file. Panics past
    /// `u32::MAX`, which no outline of a file under 4 GiB reaches.
    pub const fn in_file(&self, range: Range) -> Range {
        range.shifted(self.range.start)
    }

    /// [`Item::in_file`], `None` past `u32::MAX`.
    pub const fn checked_in_file(&self, range: Range) -> Option<Range> {
        range.checked_shifted(self.range.start)
    }

    /// The text of `range`, one of this entry's entry-relative ranges, in the file's `text`.
    pub fn text<'t>(&self, text: &'t str, range: Range) -> &'t str {
        self.in_file(range).of(text)
    }

    /// Every entry-relative range it holds that a record checks lies inside it.
    fn own_ranges(&self) -> [Option<Range>; 6] {
        let [first, second] = self.detail.ranges();
        let path = match self.visibility {
            WrittenVis::In(path) | WrittenVis::WithoutIn(path) => Some(path),
            _ => None,
        };
        [self.name, Some(self.signature), self.body, first, second, path]
    }
}

/// An attribute or a doc comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Attr {
    /// Relative to its entry's start (the file's, for the file's own).
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
    /// `#[path = ".."]`: the string literal, relative to the entry's start.
    Path { literal: Range },
    /// `#[macro_export]`, with or without arguments.
    MacroExport,
    /// Any other.
    Other,
}

byte_enum! {
    /// [`AttrKind`]'s byte in a record; a `#[path]`'s literal is a field beside it.
    pub(crate) enum AttrTag, refused as "a byte that is no attribute kind" {
        Doc = 0 => "doc",
        Cfg = 1 => "cfg",
        CfgAttr = 2 => "cfg_attr",
        Path = 3 => "path",
        MacroExport = 4 => "macro_export",
        Other = 5 => "other",
    }
}

byte_enum! {
    /// Which stage found a syntax error.
    pub enum ErrorOrigin, refused as "a byte that is no syntax error's origin" {
        Lexer = 0 => "lexer",
        Parser = 1 => "parser",
    }
}

/// A syntax error of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    /// Relative to the file's start: the token the lexer refused, or the empty range where
    /// the parser stopped expecting something.
    pub range: Range,
    pub origin: ErrorOrigin,
    /// Its message's bytes in the file's messages, which the outline's and the view's
    /// `message` read.
    pub message: Range,
}

blaze_format::record! {
    /// One entry of a file: an [`Item`], its name's text beside it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ItemRow: 3 {
        pub(crate) range: Range,
        pub(crate) name: Range,
        pub(crate) name_text: Text,
        pub(crate) signature: Range,
        pub(crate) body: Range,
        pub(crate) first: Range,
        pub(crate) second: Range,
        pub(crate) vis_path: Range,
        pub(crate) attrs: AttrSpan,
        pub(crate) parent: u32,
        pub(crate) end: u32,
        pub(crate) disambiguator: u32,
        pub(crate) syntax: u16,
        pub(crate) hash: ContentId,
        pub(crate) signature_hash: ContentId,
        pub(crate) kind: ItemKind,
        pub(crate) vis: WrittenVisTag,
        pub(crate) named: bool,
        pub(crate) bodied: bool,
        pub(crate) has_first: bool,
        pub(crate) has_second: bool,
        pub(crate) flag: bool,
        /// In what was padding (version 2 had `pad: [u8; 3]`): no byte more a row.
        pub(crate) flags: ItemFlags,
        pub(crate) pad: [u8; 2],
    }
}

blaze_format::record! {
    /// One [`Attr`].
    ///
    /// 2: `literal` of a `#[cfg]` or `#[cfg_attr]` is the attribute's own bytes, as written, in
    /// the record's names table ([`FileItemsView::cfgs`]: what plan time evaluates a unit's
    /// `test` against, D191), in what version 1 left zero: no byte more a row, and a file with no
    /// such attribute writes no byte more at all ([`FileItems::write`]).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct AttrRow: 2 {
        pub(crate) range: Range,
        /// A `#[path]`'s literal, relative to its entry; a cfg attribute's text in the names
        /// table; zero for every other kind.
        pub(crate) literal: Range,
        pub(crate) inner: bool,
        pub(crate) kind: AttrTag,
        pub(crate) pad: [u8; 2],
    }
}

blaze_format::record! {
    /// One [`SyntaxError`].
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ErrorRow: 1 {
        pub(crate) range: Range,
        pub(crate) message: Range,
        pub(crate) origin: ErrorOrigin,
        pub(crate) pad: [u8; 3],
    }
}

blaze_format::record! {
    /// The root of a [`FileItems`] buffer.
    ///
    /// 4: its attributes are [`AttrRow`] 2 (a cfg attribute's text kept). A home's outline
    /// records of form 3 are read as no record of this form: every outline is remade once, by
    /// the moved tools as well (blaze-cli home.rs `tools`).
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct FileItemsRoot: 4 {
        pub(crate) file: ContentId,
        pub(crate) items: Slice<ItemRow>,
        pub(crate) attrs: Slice<AttrRow>,
        pub(crate) errors: Slice<ErrorRow>,
        /// Each entry's name's text, in the entries' order.
        pub(crate) names: Slice<u8>,
        /// The errors' messages, as the outline holds them.
        pub(crate) messages: Slice<u8>,
        pub(crate) file_attrs: AttrSpan,
        /// The flags of the file's own tokens, outside every entry (`#![..]`).
        pub(crate) own_flags: ItemFlags,
        /// `own_flags` and every entry's together: the file's answer without its rows.
        pub(crate) flags: ItemFlags,
        pub(crate) pad: [u8; 2],
    }
}

fn row_of(item: &Item, name_text: Text) -> ItemRow {
    let (kind, payload) = item.detail.split();
    let (vis, vis_path) = item.visibility.split();
    ItemRow {
        range: item.range,
        name: item.name.unwrap_or_default(),
        name_text,
        signature: item.signature,
        body: item.body.unwrap_or_default(),
        first: payload.first.unwrap_or_default(),
        second: payload.second.unwrap_or_default(),
        vis_path: vis_path.unwrap_or_default(),
        attrs: item.attrs,
        parent: index_word(item.parent),
        end: item.end.0,
        disambiguator: item.disambiguator,
        syntax: payload.syntax,
        hash: item.hash,
        signature_hash: item.signature_hash,
        kind,
        vis,
        named: item.name.is_some(),
        bodied: item.body.is_some(),
        has_first: payload.first.is_some(),
        has_second: payload.second.is_some(),
        flag: payload.flag,
        flags: item.flags,
        pad: [0; 2],
    }
}

fn item_of(row: &ItemRow) -> Item {
    let payload = Payload {
        first: row.has_first.then_some(row.first),
        second: row.has_second.then_some(row.second),
        flag: row.flag,
        syntax: row.syntax,
    };
    Item {
        detail: ItemDetail::join(row.kind, payload),
        parent: index_of(row.parent),
        end: ItemIndex(row.end),
        range: row.range,
        name: row.named.then_some(row.name),
        visibility: WrittenVis::join(row.vis, || row.vis_path),
        signature: row.signature,
        body: row.bodied.then_some(row.body),
        attrs: row.attrs,
        disambiguator: row.disambiguator,
        hash: row.hash,
        signature_hash: row.signature_hash,
        flags: row.flags,
    }
}

/// Whether an attribute of kind `kind` keeps its text in the record ([`AttrRow`] 2).
const fn keeps_text(kind: AttrKind) -> bool {
    matches!(kind, AttrKind::Cfg | AttrKind::CfgAttr)
}

/// `attr`'s row; `text`: a cfg attribute's text in the names table ([`keeps_text`]), ignored
/// for any other kind.
fn attr_row_of(attr: &Attr, text: Range) -> AttrRow {
    let (kind, literal) = match attr.kind {
        AttrKind::Doc => (AttrTag::Doc, Range::default()),
        AttrKind::Cfg => (AttrTag::Cfg, text),
        AttrKind::CfgAttr => (AttrTag::CfgAttr, text),
        AttrKind::Path { literal } => (AttrTag::Path, literal),
        AttrKind::MacroExport => (AttrTag::MacroExport, Range::default()),
        AttrKind::Other => (AttrTag::Other, Range::default()),
    };
    AttrRow { range: attr.range, literal, inner: attr.inner, kind, pad: [0; 2] }
}

/// The text `row` keeps ([`keeps_text`]), as its row holds it; zero for any other kind.
fn kept_text(row: &AttrRow) -> Range {
    match row.kind {
        AttrTag::Cfg | AttrTag::CfgAttr => row.literal,
        _ => Range::default(),
    }
}

fn attr_of(row: &AttrRow) -> Attr {
    let kind = match row.kind {
        AttrTag::Doc => AttrKind::Doc,
        AttrTag::Cfg => AttrKind::Cfg,
        AttrTag::CfgAttr => AttrKind::CfgAttr,
        AttrTag::Path => AttrKind::Path { literal: row.literal },
        AttrTag::MacroExport => AttrKind::MacroExport,
        AttrTag::Other => AttrKind::Other,
    };
    Attr { range: row.range, inner: row.inner, kind }
}

fn error_row_of(error: &SyntaxError) -> ErrorRow {
    ErrorRow { range: error.range, message: error.message, origin: error.origin, pad: [0; 3] }
}

fn error_of(row: &ErrorRow) -> SyntaxError {
    SyntaxError { range: row.range, origin: row.origin, message: row.message }
}

/// What can be wrong with an entry: the writer refuses it, and the view refuses a buffer that
/// holds it, by the one check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    /// Its parent is not before it, or does not hold it and its descendants.
    Parent,
    /// Its descendants end before it or past the file's entries.
    End,
    /// Its range ends before it starts, or one of its own ranges runs past it.
    Range,
    /// Its attribute span runs backwards or past the file's attributes.
    Attrs,
}

impl Fault {
    fn what(self) -> &'static str {
        match self {
            Fault::Parent => "an entry whose parent is not before it or does not hold it",
            Fault::End => "an entry whose descendants end before it or past the file's entries",
            Fault::Range => "an entry with a range past itself",
            Fault::Attrs => "an entry whose attributes run past the file's",
        }
    }

    fn error(self, index: u32) -> BuildError {
        match self {
            Fault::Parent | Fault::End => BuildError::BadIndex { table: "FileItems items", index },
            Fault::Range => BuildError::PastItem { index },
            Fault::Attrs => BuildError::BadIndex { table: "FileItems attrs", index },
        }
    }
}

/// Whether `range` runs forwards.
const fn forwards(range: Range) -> bool {
    range.start <= range.end
}

/// What is wrong with the entry at `index` of `items` entries and `attrs` attributes, whose
/// parent's [`Item::end`] is `parent_end` (when it has a parent before it).
fn fault_of(
    index: u32,
    item: &Item,
    parent_end: Option<u32>,
    items: usize,
    attrs: usize,
) -> Option<Fault> {
    if let Some(parent) = item.parent {
        let holds = parent_end.is_some_and(|end| index < end && item.end.0 <= end);
        if parent.0 >= index || !holds {
            return Some(Fault::Parent);
        }
    }
    if item.end.0 <= index || wide(item.end.0) > items {
        return Some(Fault::End);
    }
    if !forwards(item.range) {
        return Some(Fault::Range);
    }
    let len = item.range.len();
    let inside = |range: Range| forwards(range) && range.end <= len;
    if !item.own_ranges().into_iter().flatten().all(inside) {
        return Some(Fault::Range);
    }
    if item.attrs.start > item.attrs.end || wide(item.attrs.end) > attrs {
        return Some(Fault::Attrs);
    }
    None
}

/// Whether an attribute's ranges run forwards.
fn attr_is_sound(attr: &Attr) -> bool {
    match attr.kind {
        AttrKind::Path { literal } => forwards(attr.range) && forwards(literal),
        _ => forwards(attr.range),
    }
}

/// A file's entries as a record is written from them: what an outline holds, and the file's
/// text, from which each entry's name is copied so the record answers names without the text.
#[derive(Clone, Copy, Debug)]
pub struct FileItems<'p> {
    /// The content of the file.
    pub file: ContentId,
    /// The file's text.
    pub text: &'p str,
    /// Every entry, in the file's order: an entry comes before the entries written inside it.
    pub items: &'p [Item],
    /// Every entry's attributes, then the file's own, as the entries' and
    /// [`FileItems::file_attrs`]'s spans name them.
    pub attrs: &'p [Attr],
    /// The file's own inner attributes and doc comments, in [`FileItems::attrs`].
    pub file_attrs: AttrSpan,
    /// The syntax errors.
    pub errors: &'p [SyntaxError],
    /// The errors' messages, one after another, each a whole `str`.
    pub messages: &'p [u8],
    /// The flags of the file's own tokens, outside every entry.
    pub file_flags: ItemFlags,
}

impl FileItems<'_> {
    /// The record's buffer, in `alloc`. Refused when an entry's parent or descendants are not
    /// where they are said to be, a range runs backwards or past its entry, a name is not in
    /// the text, or a message is not one.
    pub fn write<'a>(&self, alloc: Alloc<'a>) -> Result<RecordBytes<'a>, BuildError> {
        count(self.items.len(), "FileItems items")?;
        count(self.attrs.len(), "FileItems attrs")?;
        count(self.errors.len(), "FileItems errors")?;
        let mut names = Strings::new_in(alloc);
        let mut rows = Vec::with_capacity_in(self.items.len(), alloc);
        let mut flags = self.file_flags;
        for (at, item) in self.items.iter().enumerate() {
            let index = count(at, "FileItems items")?;
            flags |= item.flags;
            let parent_end = item.parent.and_then(|parent| self.items.get(parent.at())).map(|parent| parent.end.0);
            if let Some(fault) = fault_of(index, item, parent_end, self.items.len(), self.attrs.len()) {
                return Err(fault.error(index));
            }
            let name = match item.name {
                None => "",
                Some(name) => item
                    .checked_in_file(name)
                    .and_then(|name| name.get(self.text))
                    .ok_or(BuildError::PastItem { index })?,
            };
            rows.push(row_of(item, names.push(name, "FileItems names")?));
        }
        if self.file_attrs.start > self.file_attrs.end || wide(self.file_attrs.end) > self.attrs.len() {
            return Err(BuildError::BadIndex { table: "FileItems attrs", index: self.file_attrs.end });
        }
        // Where each attribute's range counts from (its entry's start, the file's for the file's
        // own): asked only of a cfg attribute, whose own bytes go into the names table after
        // every name, in the attributes' order (rule 25). A file with none makes no list.
        let mut bases: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
        if self.attrs.iter().any(|attr| keeps_text(attr.kind)) {
            bases.resize(self.attrs.len(), u32::MAX);
            for item in self.items.iter() {
                bases[item.attrs.to_usize()].fill(item.range.start);
            }
            bases[self.file_attrs.to_usize()].fill(0);
        }
        let mut attrs = Vec::with_capacity_in(self.attrs.len(), alloc);
        for (at, attr) in self.attrs.iter().enumerate() {
            let index = count(at, "FileItems attrs")?;
            if !attr_is_sound(attr) {
                return Err(BuildError::BadIndex { table: "FileItems attrs", index });
            }
            let text = match keeps_text(attr.kind) {
                false => Range::default(),
                true => {
                    // An attribute of no entry, or whose bytes are not the text's, is refused.
                    let base = bases.get(at).copied().filter(|base| *base != u32::MAX).ok_or(BuildError::BadIndex { table: "FileItems attrs", index })?;
                    let written = attr.range.checked_shifted(base).and_then(|range| range.get(self.text)).ok_or(BuildError::BadIndex { table: "FileItems attrs", index })?;
                    let kept = names.push(written, "FileItems names")?;
                    Range::new(kept.at, kept.at + kept.len)
                }
            };
            attrs.push(attr_row_of(attr, text));
        }
        let messages = core::str::from_utf8(self.messages)
            .map_err(|_| BuildError::BadIndex { table: "FileItems messages", index: 0 })?;
        let mut errors = Vec::with_capacity_in(self.errors.len(), alloc);
        for (at, error) in self.errors.iter().enumerate() {
            if !forwards(error.range) || error.message.get(messages).is_none() {
                return Err(BuildError::BadIndex { table: "FileItems errors", index: count(at, "FileItems errors")? });
            }
            errors.push(error_row_of(error));
        }
        let mut out = Writer::new(WriterOpts { schema: <FileItemsRoot as blaze_format::Record>::SCHEMA, alloc: alloc, capacity: None });
        let names = Column::push(&mut out, names.bytes());
        let messages = Column::push(&mut out, messages.as_bytes());
        let items = Column::push(&mut out, &rows);
        let attrs = Column::push(&mut out, &attrs);
        let errors = Column::push(&mut out, &errors);
        let file = self.file;
        let file_attrs = self.file_attrs;
        let own_flags = self.file_flags;
        Ok(finish_with(out, |at| FileItemsRoot {
            file,
            items: items.slice(at, offset_of!(FileItemsRoot, items)),
            attrs: attrs.slice(at, offset_of!(FileItemsRoot, attrs)),
            errors: errors.slice(at, offset_of!(FileItemsRoot, errors)),
            names: names.slice(at, offset_of!(FileItemsRoot, names)),
            messages: messages.slice(at, offset_of!(FileItemsRoot, messages)),
            file_attrs,
            own_flags,
            flags,
            pad: [0; 2],
        }))
    }
}

/// An entry of a [`FileItemsView`]: the [`Item`], and its name's text. It derefs to the item,
/// so `entry.range`, `entry.kind()` and the rest read as on an outline's; `entry.name` is the
/// text, `entry.item.name` the range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileItem<'a> {
    /// Its name as written (`r#` included); empty when it has none.
    pub name: &'a str,
    pub item: Item,
}

impl Deref for FileItem<'_> {
    type Target = Item;

    fn deref(&self) -> &Item {
        &self.item
    }
}

/// A [`FileItems`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct FileItemsView<'a> {
    file: ContentId,
    items: &'a [ItemRow],
    attrs: &'a [AttrRow],
    file_attrs: AttrSpan,
    errors: &'a [ErrorRow],
    names: &'a str,
    messages: &'a str,
    own_flags: ItemFlags,
    flags: ItemFlags,
}

impl<'a> FileItemsView<'a> {
    /// The record in `buffer`, checked once: its layout, that each row is the one encoding of
    /// its value, every name and message, every entry's parent, descendants and ranges, and
    /// every attribute span.
    pub fn read(buffer: &'a [u8]) -> Result<FileItemsView<'a>, FormatError> {
        let root = blaze_format::open::<FileItemsRoot>(buffer)?;
        let view = FileItemsView {
            file: root.file,
            items: root.items.follow(buffer)?,
            attrs: root.attrs.follow(buffer)?,
            file_attrs: root.file_attrs,
            errors: root.errors.follow(buffer)?,
            names: root.names.as_str(buffer)?,
            messages: root.messages.as_str(buffer)?,
            own_flags: root.own_flags,
            flags: root.flags,
        };
        let invalid = |row_at: u64, what: &'static str| FormatError::Invalid { at: row_at, what };
        if root.pad != [0; 2] {
            return Err(invalid(0, "a file's root whose padding is not zero"));
        }
        let mut flags = view.own_flags;
        for (at, row) in view.items.iter().enumerate() {
            flags |= row.flags;
            let index = u32::try_from(at).expect("a slice's length is a u32");
            let row_at = at_of_record(row, buffer);
            check_text(view.names, row.name_text, row, buffer)?;
            let item = item_of(row);
            if row_of(&item, row.name_text) != *row {
                return Err(invalid(row_at, "an entry whose bytes are not its value's one encoding"));
            }
            if row.name_text.len != item.name.map_or(0, Range::len) {
                return Err(invalid(row_at, "an entry whose name's text is not its name's length"));
            }
            let parent_end = item.parent.and_then(|parent| view.items.get(parent.at())).map(|parent| parent.end);
            if let Some(fault) = fault_of(index, &item, parent_end, view.items.len(), view.attrs.len()) {
                return Err(invalid(row_at, fault.what()));
            }
        }
        for row in view.attrs {
            let attr = attr_of(row);
            let text = kept_text(row);
            if attr_row_of(&attr, text) != *row || !attr_is_sound(&attr) {
                return Err(invalid(at_of_record(row, buffer), "an attribute that is not its value's one sound encoding"));
            }
            // A cfg attribute's text is its own bytes: in the names table, as long as its range.
            if keeps_text(attr.kind) && (!forwards(text) || text.get(view.names).is_none() || text.len() != attr.range.len()) {
                return Err(invalid(at_of_record(row, buffer), "a cfg attribute whose text is not in the names table at its range's length"));
            }
        }
        let span = view.file_attrs;
        if span.start > span.end || wide(span.end) > view.attrs.len() {
            return Err(invalid(0, "the file's attributes run past the record's"));
        }
        for row in view.errors {
            let error = error_of(row);
            let sound = forwards(error.range) && error.message.get(view.messages).is_some();
            if error_row_of(&error) != *row || !sound {
                return Err(invalid(at_of_record(row, buffer), "a syntax error that is not its value's one sound encoding"));
            }
        }
        if flags != view.flags {
            return Err(invalid(0, "a file's flags that are not its own and its entries' together"));
        }
        Ok(view)
    }

    /// The content of the file these are the entries of.
    pub fn file(&self) -> ContentId {
        self.file
    }

    /// Every flag the file's tokens set, its own and its entries': read from the root alone,
    /// so "none set" costs no row.
    pub fn flags(&self) -> ItemFlags {
        self.flags
    }

    /// The flags of the file's own tokens, outside every entry (`#![..]`).
    pub fn own_flags(&self) -> ItemFlags {
        self.own_flags
    }

    /// Each entry with a flag set, in order, with its flags; nothing, without a row read, when
    /// the file sets none.
    pub fn flagged(&self) -> impl Iterator<Item = (ItemIndex, ItemFlags)> + use<'a> {
        let rows: &'a [ItemRow] = if self.flags.is_empty() { &[] } else { self.items };
        rows.iter().enumerate().filter(|(_, row)| !row.flags.is_empty()).map(|(at, row)| {
            (ItemIndex(u32::try_from(at).expect("a record's entries are counted in u32")), row.flags)
        })
    }

    /// How many entries the file has.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the file has no entry.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn entry(&self, at: usize) -> FileItem<'a> {
        let row = &self.items[at];
        FileItem { name: text_of(self.names, row.name_text), item: item_of(row) }
    }

    /// The entry at `index`, if the file has one there.
    pub fn item(&self, index: ItemIndex) -> Option<FileItem<'a>> {
        (index.at() < self.items.len()).then(|| self.entry(index.at()))
    }

    /// Every entry, in the file's order: an entry comes before the entries written inside it.
    pub fn items(&self) -> impl ExactSizeIterator<Item = FileItem<'a>> + use<'a> {
        let view = *self;
        (0..self.items.len()).map(move |at| view.entry(at))
    }

    /// The entries written directly in `parent` (in the file itself, for `None`), in order.
    /// Panics when `parent` is not an entry of this file.
    pub fn children(&self, parent: Option<ItemIndex>) -> ChildItems<'a> {
        let (at, end) = match parent {
            None => (0, self.items.len()),
            Some(parent) => (parent.at() + 1, wide(self.items[parent.at()].end)),
        };
        ChildItems { view: *self, at, end }
    }

    fn span(&self, span: AttrSpan) -> impl ExactSizeIterator<Item = Attr> + use<'a> {
        let attrs: &'a [AttrRow] = self.attrs;
        attrs[span.to_usize()].iter().map(attr_of)
    }

    /// The attributes and doc comments of `item`, an entry of this file. Panics when its span
    /// is past this file's attributes, as it is for no entry of this view.
    pub fn attrs(&self, item: &Item) -> impl ExactSizeIterator<Item = Attr> + use<'a> {
        self.span(item.attrs)
    }

    /// The bytes of `item`'s outer attributes and doc comments, relative to its start; empty
    /// at its start when it has none.
    pub fn outer_attrs(&self, item: &Item) -> Range {
        let mut outer = self.attrs(item).take_while(|attr| !attr.inner);
        match outer.next() {
            None => Range::new(0, 0),
            Some(first) => Range::new(first.range.start, outer.last().unwrap_or(first).range.end),
        }
    }

    /// The file's own inner attributes and doc comments (`#![..]`, `//!`).
    pub fn file_attrs(&self) -> impl ExactSizeIterator<Item = Attr> + use<'a> {
        self.span(self.file_attrs)
    }

    /// Each `#[cfg(..)]` and `#[cfg_attr(..)]` of `item` (of the file itself, its `#![..]`, for
    /// `None`), in order: its kind and its own bytes as written, `#[` to `]`. What plan time
    /// reads a unit's `test` against (blaze-cli engine_plan_cfg.rs), with no text of the file.
    pub fn cfgs(&self, item: Option<&Item>) -> impl Iterator<Item = (AttrKind, &'a str)> + use<'a> {
        let span = item.map_or(self.file_attrs, |item| item.attrs);
        let names = self.names;
        let attrs: &'a [AttrRow] = self.attrs;
        attrs[span.to_usize()].iter().filter_map(move |row| {
            let attr = attr_of(row);
            match keeps_text(attr.kind) {
                true => Some((attr.kind, kept_text(row).get(names).expect("a cfg attribute's text is checked when its view is read"))),
                false => None,
            }
        })
    }

    /// How many bytes the names table holds: what a test of [`AttrRow`] 2's cost reads.
    #[cfg(test)]
    pub(crate) fn names_len(&self) -> usize {
        self.names.len()
    }

    /// The syntax errors: the parser's, in the order it found them, then the lexer's.
    pub fn errors(&self) -> impl ExactSizeIterator<Item = SyntaxError> + use<'a> {
        let errors: &'a [ErrorRow] = self.errors;
        errors.iter().map(error_of)
    }

    /// What `error`, one of [`FileItemsView::errors`], says. Panics for an error of another
    /// record whose message is past this one's.
    pub fn message(&self, error: &SyntaxError) -> &'a str {
        error.message.get(self.messages).expect("an error of this record, its message checked when read")
    }
}

/// The entries written directly in one entry or in the file ([`FileItemsView::children`]).
#[derive(Clone, Copy, Debug)]
pub struct ChildItems<'a> {
    view: FileItemsView<'a>,
    at: usize,
    end: usize,
}

impl<'a> Iterator for ChildItems<'a> {
    type Item = (ItemIndex, FileItem<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.end {
            return None;
        }
        let entry = self.view.entry(self.at);
        let index = ItemIndex(u32::try_from(self.at).expect("a record's entries are counted in u32"));
        // A child's descendants follow it: the next child is past them. The view checked that
        // every entry's end is past it, so this always moves on.
        self.at = entry.end.at();
        Some((index, entry))
    }
}
