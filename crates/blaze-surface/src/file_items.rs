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

use core::alloc::Allocator;
use core::fmt;
use core::mem::offset_of;
use core::ops::Deref;

use blaze_format::{FormatError, Slice, Writer};
use blaze_id::{ContentId, Portable};
use blaze_own::Vec;

use crate::column::{
    Column, RecordBytes, Strings, Text, at_of, at_of_record, check_text, count, finish_with,
    index_of, index_word, text_of, wide,
};
use crate::error::BuildError;
// `byte_enum!` names `fmt`, `Portable`, `FormatError` and `at_of` where it is called.
use crate::kind::{ItemIndex, ItemKind, Range, WrittenVis, WrittenVisTag, byte_enum};

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
                None => none,
                Some(Rename::Name(name)) => Payload { first: Some(name), ..none },
                Some(Rename::Underscore) => Payload { flag: true, ..none },
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
                    (Some(name), _) => Some(Rename::Name(name)),
                    (None, true) => Some(Rename::Underscore),
                    (None, false) => None,
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

/// An `extern crate`'s `as ..`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rename {
    /// `as name`: the name.
    Name(Range),
    /// `as _`.
    Underscore,
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
    /// Its tokens' [`ContentId`] in blaze-file-items' `ITEM_DOMAIN`.
    pub hash: ContentId,
    /// Its tokens outside its body, in blaze-file-items' `SIGNATURE_DOMAIN`.
    pub signature_hash: ContentId,
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
    pub(crate) struct ItemRow: 2 {
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
        pub(crate) pad: [u8; 3],
    }
}

blaze_format::record! {
    /// One [`Attr`].
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct AttrRow: 1 {
        pub(crate) range: Range,
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
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct FileItemsRoot: 2 {
        pub(crate) file: ContentId,
        pub(crate) items: Slice<ItemRow>,
        pub(crate) attrs: Slice<AttrRow>,
        pub(crate) errors: Slice<ErrorRow>,
        /// Each entry's name's text, in the entries' order.
        pub(crate) names: Slice<u8>,
        /// The errors' messages, as the outline holds them.
        pub(crate) messages: Slice<u8>,
        pub(crate) file_attrs: AttrSpan,
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
        pad: [0; 3],
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
    }
}

fn attr_row_of(attr: &Attr) -> AttrRow {
    let (kind, literal) = match attr.kind {
        AttrKind::Doc => (AttrTag::Doc, Range::default()),
        AttrKind::Cfg => (AttrTag::Cfg, Range::default()),
        AttrKind::CfgAttr => (AttrTag::CfgAttr, Range::default()),
        AttrKind::Path { literal } => (AttrTag::Path, literal),
        AttrKind::MacroExport => (AttrTag::MacroExport, Range::default()),
        AttrKind::Other => (AttrTag::Other, Range::default()),
    };
    AttrRow { range: attr.range, literal, inner: attr.inner, kind, pad: [0; 2] }
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
}

impl FileItems<'_> {
    /// The record's buffer, in `alloc`. Refused when an entry's parent or descendants are not
    /// where they are said to be, a range runs backwards or past its entry, a name is not in
    /// the text, or a message is not one.
    pub fn write<A: Allocator + Clone>(&self, alloc: A) -> Result<RecordBytes<A>, BuildError> {
        count(self.items.len(), "FileItems items")?;
        count(self.attrs.len(), "FileItems attrs")?;
        count(self.errors.len(), "FileItems errors")?;
        let mut names = Strings::new_in(alloc.clone());
        let mut rows = Vec::with_capacity_in(self.items.len(), alloc.clone());
        for (at, item) in self.items.iter().enumerate() {
            let index = count(at, "FileItems items")?;
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
        let mut attrs = Vec::with_capacity_in(self.attrs.len(), alloc.clone());
        for (at, attr) in self.attrs.iter().enumerate() {
            if !attr_is_sound(attr) {
                return Err(BuildError::BadIndex { table: "FileItems attrs", index: count(at, "FileItems attrs")? });
            }
            attrs.push(attr_row_of(attr));
        }
        if self.file_attrs.start > self.file_attrs.end || wide(self.file_attrs.end) > self.attrs.len() {
            return Err(BuildError::BadIndex { table: "FileItems attrs", index: self.file_attrs.end });
        }
        let messages = core::str::from_utf8(self.messages)
            .map_err(|_| BuildError::BadIndex { table: "FileItems messages", index: 0 })?;
        let mut errors = Vec::with_capacity_in(self.errors.len(), alloc.clone());
        for (at, error) in self.errors.iter().enumerate() {
            if !forwards(error.range) || error.message.get(messages).is_none() {
                return Err(BuildError::BadIndex { table: "FileItems errors", index: count(at, "FileItems errors")? });
            }
            errors.push(error_row_of(error));
        }
        let mut out = Writer::new_in(<FileItemsRoot as blaze_format::Record>::SCHEMA, alloc);
        let names = Column::push(&mut out, names.bytes());
        let messages = Column::push(&mut out, messages.as_bytes());
        let items = Column::push(&mut out, &rows);
        let attrs = Column::push(&mut out, &attrs);
        let errors = Column::push(&mut out, &errors);
        let file = self.file;
        let file_attrs = self.file_attrs;
        Ok(finish_with(out, |at| FileItemsRoot {
            file,
            items: items.slice(at, offset_of!(FileItemsRoot, items)),
            attrs: attrs.slice(at, offset_of!(FileItemsRoot, attrs)),
            errors: errors.slice(at, offset_of!(FileItemsRoot, errors)),
            names: names.slice(at, offset_of!(FileItemsRoot, names)),
            messages: messages.slice(at, offset_of!(FileItemsRoot, messages)),
            file_attrs,
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
        };
        let invalid = |row_at: u64, what: &'static str| FormatError::Invalid { at: row_at, what };
        for (at, row) in view.items.iter().enumerate() {
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
            if attr_row_of(&attr) != *row || !attr_is_sound(&attr) {
                return Err(invalid(at_of_record(row, buffer), "an attribute that is not its value's one sound encoding"));
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
        Ok(view)
    }

    /// The content of the file these are the entries of.
    pub fn file(&self) -> ContentId {
        self.file
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
