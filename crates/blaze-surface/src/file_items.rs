//! [`FileItems`]: one file's items, in the file's order, keyed by the file's bytes, its edition
//! and the outline's version. No tree is kept: a later question re-reads an item's bytes by its
//! ranges.

use core::alloc::Allocator;
use core::mem::offset_of;

use blaze_format::{FormatError, Slice, Writer};
use blaze_id::ContentId;

use crate::column::{
    Column, NONE, Strings, Text, check_index, check_text, count, finish_with, index_of,
    index_word, text_of,
};
use crate::error::BuildError;
use crate::kind::{ByteRange, ItemIndex, ItemKind};

blaze_format::record! {
    /// One item of a file.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ItemRow: 1 {
        pub(crate) name: Text,
        pub(crate) at: u32,
        pub(crate) len: u32,
        pub(crate) signature: ByteRange,
        pub(crate) body: ByteRange,
        pub(crate) attrs: ByteRange,
        pub(crate) parent: u32,
        pub(crate) kind: ItemKind,
        pub(crate) pad: [u8; 3],
    }
}

blaze_format::record! {
    /// One `mod name;` or `mod name { .. }`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ModRow: 1 {
        pub(crate) name: Text,
        pub(crate) item: u32,
        pub(crate) inline: bool,
        pub(crate) pad: [u8; 3],
    }
}

blaze_format::record! {
    /// One leaf of a `use` tree.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct UseRow: 1 {
        pub(crate) path: Text,
        pub(crate) alias: Text,
        pub(crate) item: u32,
        pub(crate) glob: bool,
        pub(crate) aliased: bool,
        pub(crate) pad: [u8; 2],
    }
}

blaze_format::record! {
    /// One item-level macro call.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct CallRow: 1 {
        pub(crate) path: Text,
        pub(crate) at: u32,
        pub(crate) len: u32,
        pub(crate) parent: u32,
    }
}

blaze_format::record! {
    /// The root of a [`FileItems`] buffer.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct FileItemsRoot: 1 {
        pub(crate) file: ContentId,
        pub(crate) items: Slice<ItemRow>,
        pub(crate) mods: Slice<ModRow>,
        pub(crate) uses: Slice<UseRow>,
        pub(crate) calls: Slice<CallRow>,
        pub(crate) strings: Slice<u8>,
    }
}

/// One item of a file.
///
/// `at` is file-relative: the record is keyed by the file's bytes, so it moves exactly when the
/// key does, and it is what slicing the file's text needs. The three ranges are item-relative,
/// counted from `at` (rule 71): a later record that copies one keeps it when an edit above
/// moves the item. The item starts at its first outer attribute or doc comment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileItem<'a> {
    pub kind: ItemKind,
    /// Its name; empty for an `impl` and a `use` (whose leaves are [`UseLeaf`]s).
    pub name: &'a str,
    /// Its first byte in the file.
    pub at: u32,
    /// How many bytes it covers.
    pub len: u32,
    /// Its signature: everything but its body and its attributes.
    pub signature: ByteRange,
    /// Its body (a block, a field list, an item list); [`ByteRange::EMPTY`] when it has none.
    pub body: ByteRange,
    /// Its outer attributes and doc comments, whose tokens a later question lexes from them.
    pub attrs: ByteRange,
    /// The item it is written inside (an inline module, an impl, a trait, an enum, a struct for
    /// a field, an extern block's items' module), always before it; `None` at the file's top.
    pub parent: Option<ItemIndex>,
}

/// One `mod` of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModDecl<'a> {
    pub name: &'a str,
    /// The module's own item row.
    pub item: ItemIndex,
    /// `mod name { .. }`, whose items are rows of this file; else `mod name;`, whose file is
    /// the module's [`crate::ModuleFile`].
    pub inline: bool,
}

/// One leaf of a `use` tree: `use a::{b, c as d, e::*}` is three.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UseLeaf<'a> {
    /// The path as written, its segments joined by `::`; a glob's path is its module's.
    pub path: &'a str,
    /// `as name`, `as _` included.
    pub alias: Option<&'a str>,
    pub glob: bool,
    /// The `use` item it is a leaf of.
    pub item: ItemIndex,
}

/// One item-level macro call: its items are its `Expansion`'s, not rows of this file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroCall<'a> {
    /// The macro's path as written, `!` cut.
    pub path: &'a str,
    /// File-relative, as [`FileItem::at`].
    pub at: u32,
    pub len: u32,
    /// The item it is written inside, as [`FileItem::parent`].
    pub parent: Option<ItemIndex>,
}

/// The writer of a file's [`FileItems`] record. Rows are given in the file's order, which fixes
/// the record's bytes; the string table is laid out at the end in a fixed table order, so the
/// order the four kinds of row are interleaved in does not.
pub struct FileItems<A: Allocator + Clone> {
    file: ContentId,
    items: Vec<ItemRow, A>,
    mods: Vec<ModRow, A>,
    uses: Vec<UseRow, A>,
    calls: Vec<CallRow, A>,
    staging: Strings<A>,
    alloc: A,
}

impl<A: Allocator + Clone> FileItems<A> {
    /// A writer of the items of the file whose content is `file`, its tables in `alloc`.
    pub fn new_in(file: ContentId, alloc: A) -> FileItems<A> {
        FileItems {
            file,
            items: Vec::new_in(alloc.clone()),
            mods: Vec::new_in(alloc.clone()),
            uses: Vec::new_in(alloc.clone()),
            calls: Vec::new_in(alloc.clone()),
            staging: Strings::new_in(alloc.clone()),
            alloc,
        }
    }

    /// Adds the next item of the file, giving back its index. Refused when its parent is not an
    /// earlier item or a range runs past the item.
    pub fn item(&mut self, item: FileItem<'_>) -> Result<ItemIndex, BuildError> {
        let index = count(self.items.len(), "FileItems items")?;
        if let Some(parent) = item.parent
            && parent.0 >= index
        {
            return Err(BuildError::BadIndex { table: "FileItems items", index: parent.0 });
        }
        item.at.checked_add(item.len).ok_or(BuildError::PastItem { index })?;
        for range in [item.signature, item.body, item.attrs] {
            if range.end().is_none_or(|end| end > item.len) {
                return Err(BuildError::PastItem { index });
            }
        }
        let name = self.staging.push(item.name, "FileItems strings")?;
        self.items.push(ItemRow {
            name,
            at: item.at,
            len: item.len,
            signature: item.signature,
            body: item.body,
            attrs: item.attrs,
            parent: index_word(item.parent),
            kind: item.kind,
            pad: [0; 3],
        });
        Ok(ItemIndex(index))
    }

    /// Adds a `mod` of an item already added.
    pub fn module(&mut self, module: ModDecl<'_>) -> Result<(), BuildError> {
        self.check_item(module.item, "FileItems mods")?;
        let name = self.staging.push(module.name, "FileItems strings")?;
        self.mods.push(ModRow { name, item: module.item.0, inline: module.inline, pad: [0; 3] });
        Ok(())
    }

    /// Adds a leaf of a `use` item already added.
    pub fn use_leaf(&mut self, leaf: UseLeaf<'_>) -> Result<(), BuildError> {
        self.check_item(leaf.item, "FileItems uses")?;
        let path = self.staging.push(leaf.path, "FileItems strings")?;
        let alias = self.staging.push(leaf.alias.unwrap_or(""), "FileItems strings")?;
        self.uses.push(UseRow {
            path,
            alias,
            item: leaf.item.0,
            glob: leaf.glob,
            aliased: leaf.alias.is_some(),
            pad: [0; 2],
        });
        Ok(())
    }

    /// Adds an item-level macro call, inside an item already added or at the file's top.
    pub fn macro_call(&mut self, call: MacroCall<'_>) -> Result<(), BuildError> {
        if let Some(parent) = call.parent {
            self.check_item(parent, "FileItems calls")?;
        }
        let path = self.staging.push(call.path, "FileItems strings")?;
        self.calls.push(CallRow { path, at: call.at, len: call.len, parent: index_word(call.parent) });
        Ok(())
    }

    fn check_item(&self, item: ItemIndex, table: &'static str) -> Result<(), BuildError> {
        match crate::column::wide(item.0) < self.items.len() {
            true => Ok(()),
            false => Err(BuildError::BadIndex { table, index: item.0 }),
        }
    }

    /// The record's buffer, in the writer's allocator.
    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let mut strings = Strings::new_in(self.alloc.clone());
        const TABLE: &str = "FileItems strings";
        for row in &mut self.items {
            row.name = strings.moved(&self.staging, row.name, TABLE)?;
        }
        for row in &mut self.mods {
            row.name = strings.moved(&self.staging, row.name, TABLE)?;
        }
        for row in &mut self.uses {
            row.path = strings.moved(&self.staging, row.path, TABLE)?;
            row.alias = strings.moved(&self.staging, row.alias, TABLE)?;
        }
        for row in &mut self.calls {
            row.path = strings.moved(&self.staging, row.path, TABLE)?;
        }
        let mut out = Writer::new_in(<FileItemsRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let text = Column::push(&mut out, strings.bytes());
        let items = Column::push(&mut out, &self.items);
        let mods = Column::push(&mut out, &self.mods);
        let uses = Column::push(&mut out, &self.uses);
        let calls = Column::push(&mut out, &self.calls);
        Ok(finish_with(out, |at| FileItemsRoot {
            file: self.file,
            items: items.slice(at, offset_of!(FileItemsRoot, items)),
            mods: mods.slice(at, offset_of!(FileItemsRoot, mods)),
            uses: uses.slice(at, offset_of!(FileItemsRoot, uses)),
            calls: calls.slice(at, offset_of!(FileItemsRoot, calls)),
            strings: text.slice(at, offset_of!(FileItemsRoot, strings)),
        }))
    }
}

/// A [`FileItems`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct FileItemsView<'a> {
    file: ContentId,
    items: &'a [ItemRow],
    mods: &'a [ModRow],
    uses: &'a [UseRow],
    calls: &'a [CallRow],
    strings: &'a str,
}

impl<'a> FileItemsView<'a> {
    /// The record in `buffer`, checked once: its layout, every name, and every row index.
    pub fn read(buffer: &'a [u8]) -> Result<FileItemsView<'a>, FormatError> {
        let root = blaze_format::open::<FileItemsRoot>(buffer)?;
        let view = FileItemsView {
            file: root.file,
            items: root.items.follow(buffer)?,
            mods: root.mods.follow(buffer)?,
            uses: root.uses.follow(buffer)?,
            calls: root.calls.follow(buffer)?,
            strings: root.strings.as_str(buffer)?,
        };
        let len = view.items.len();
        for (index, row) in view.items.iter().enumerate() {
            check_text(view.strings, row.name, row, buffer)?;
            if row.parent != NONE && crate::column::wide(row.parent) >= index {
                return Err(FormatError::Invalid {
                    at: crate::column::at_of_record(row, buffer),
                    what: "an item whose parent is not before it",
                });
            }
        }
        for row in view.mods {
            check_text(view.strings, row.name, row, buffer)?;
            check_index(row.item, len, row, buffer, "a mod of no item")?;
        }
        for row in view.uses {
            check_text(view.strings, row.path, row, buffer)?;
            check_text(view.strings, row.alias, row, buffer)?;
            check_index(row.item, len, row, buffer, "a use leaf of no item")?;
        }
        for row in view.calls {
            check_text(view.strings, row.path, row, buffer)?;
            check_index(row.parent, len, row, buffer, "a macro call inside no item")?;
        }
        Ok(view)
    }

    /// The content of the file these are the items of.
    pub fn file(&self) -> ContentId {
        self.file
    }

    /// How many items the file has.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The item at `index`, if the file has one there.
    pub fn item(&self, index: ItemIndex) -> Option<FileItem<'a>> {
        let row = self.items.get(usize::try_from(index.0).ok()?)?;
        Some(FileItem {
            kind: row.kind,
            name: text_of(self.strings, row.name),
            at: row.at,
            len: row.len,
            signature: row.signature,
            body: row.body,
            attrs: row.attrs,
            parent: index_of(row.parent),
        })
    }

    /// The items, in the file's order.
    pub fn items(&self) -> impl Iterator<Item = FileItem<'a>> + use<'a> {
        let view = *self;
        (0..self.items.len()).filter_map(move |index| {
            view.item(ItemIndex(u32::try_from(index).ok()?))
        })
    }

    /// The `mod`s, in the file's order.
    pub fn mods(&self) -> impl Iterator<Item = ModDecl<'a>> + use<'a> {
        let strings = self.strings;
        self.mods.iter().map(move |row| ModDecl {
            name: text_of(strings, row.name),
            item: ItemIndex(row.item),
            inline: row.inline,
        })
    }

    /// The `use` leaves, in the file's order.
    pub fn uses(&self) -> impl Iterator<Item = UseLeaf<'a>> + use<'a> {
        let strings = self.strings;
        self.uses.iter().map(move |row| UseLeaf {
            path: text_of(strings, row.path),
            alias: row.aliased.then(|| text_of(strings, row.alias)),
            glob: row.glob,
            item: ItemIndex(row.item),
        })
    }

    /// The item-level macro calls, in the file's order.
    pub fn calls(&self) -> impl Iterator<Item = MacroCall<'a>> + use<'a> {
        let strings = self.strings;
        self.calls.iter().map(move |row| MacroCall {
            path: text_of(strings, row.path),
            at: row.at,
            len: row.len,
            parent: index_of(row.parent),
        })
    }
}
