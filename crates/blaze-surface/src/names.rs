//! The other records name resolution reads (docs/v2/plan/design/bottom.md, "The other records
//! names need"): [`TextualEntry`] and [`ModuleFile`]; the crate root
//! ([`crate::CrateRootBuilder`]) and [`crate::MacroDef`] are in their own modules.

use core::mem::offset_of;

use blaze_format::{FormatError, Slice, Writer, WriterOpts};
use blaze_id::{ContentId, ItemKey};
use blaze_own::{AVec, Alloc};

use crate::column::{
    Column, RecordBytes, Strings, Text, check_increasing, check_text, finish_with, index_of,
    index_word, text_of,
};
use crate::error::BuildError;
use crate::kind::ItemIndex;

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct MacroRow: 1 {
        pub(crate) name: Text,
        pub(crate) def: ContentId,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct TextualEntryRoot: 1 {
        pub(crate) module: ContentId,
        pub(crate) macros: Slice<MacroRow>,
        pub(crate) strings: Slice<u8>,
    }
}

/// The writer of a module's [`TextualEntry`]: the `macro_rules!` scope at the module's `mod`
/// line, with the macros of earlier `#[macro_use]` siblings, in textual order. The order is the
/// source's, so it fixes the bytes; a later definition of a name shadows an earlier one.
pub struct TextualEntry<'a> {
    module: ItemKey,
    rows: AVec<'a, MacroRow>,
    staging: Strings<'a>,
    alloc: Alloc<'a>,
}

impl<'a> TextualEntry<'a> {
    pub fn new_in(module: ItemKey, alloc: Alloc<'a>) -> TextualEntry<'a> {
        TextualEntry { module, rows: AVec::new_in(alloc), staging: Strings::new_in(alloc), alloc }
    }

    /// The next `macro_rules!` in textual order: its name and its [`crate::MacroDef`]'s item.
    pub fn push(&mut self, name: &str, def: ItemKey) -> Result<(), BuildError> {
        let name = self.staging.push(name, "TextualEntry strings")?;
        self.rows.push(MacroRow { name, def: def.0 });
        Ok(())
    }

    pub fn finish(mut self) -> Result<RecordBytes<'a>, BuildError> {
        let mut strings = Strings::new_in(self.alloc);
        for row in &mut self.rows {
            row.name = strings.moved(&self.staging, row.name, "TextualEntry strings")?;
        }
        let mut out =
            Writer::new(WriterOpts { schema: <TextualEntryRoot as blaze_format::Record>::SCHEMA, alloc: self.alloc, capacity: None });
        let text = Column::push(&mut out, strings.bytes());
        let macros = Column::push(&mut out, &self.rows);
        Ok(finish_with(out, |at| TextualEntryRoot {
            module: self.module.0,
            macros: macros.slice(at, offset_of!(TextualEntryRoot, macros)),
            strings: text.slice(at, offset_of!(TextualEntryRoot, strings)),
        }))
    }
}

/// A [`TextualEntry`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct TextualEntryView<'a> {
    module: ItemKey,
    rows: &'a [MacroRow],
    strings: &'a str,
}

impl<'a> TextualEntryView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<TextualEntryView<'a>, FormatError> {
        let root = blaze_format::open::<TextualEntryRoot>(buffer)?;
        let view = TextualEntryView {
            module: ItemKey(root.module),
            rows: root.macros.follow(buffer)?,
            strings: root.strings.as_str(buffer)?,
        };
        for row in view.rows {
            check_text(view.strings, row.name, row, buffer)?;
        }
        Ok(view)
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    /// The macro `name` names at the module's entry: the last of that name.
    pub fn get(&self, name: &str) -> Option<ItemKey> {
        let strings = self.strings;
        let row = self.rows.iter().rev().find(|row| text_of(strings, row.name) == name)?;
        Some(ItemKey(row.def))
    }

    /// Every `macro_rules!` in scope, in textual order.
    pub fn macros(&self) -> impl Iterator<Item = (&'a str, ItemKey)> + use<'a> {
        let strings = self.strings;
        self.rows.iter().map(move |row| (text_of(strings, row.name), ItemKey(row.def)))
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ModuleFileRoot: 1 {
        pub(crate) module: ContentId,
        pub(crate) file: ContentId,
        pub(crate) path: Text,
        pub(crate) inline: u32,
        pub(crate) absent: Slice<Text>,
        pub(crate) strings: Slice<u8>,
    }
}

/// The writer of a module's [`ModuleFile`]: which file holds its items, and the candidate paths
/// read and found absent (`foo.rs` against `foo/mod.rs`), so a file added at one of them moves
/// the record's read list.
pub struct ModuleFile<'a> {
    module: ItemKey,
    file: ContentId,
    path: Text,
    inline: Option<ItemIndex>,
    absent: AVec<'a, Text>,
    staging: Strings<'a>,
    alloc: Alloc<'a>,
}

impl<'a> ModuleFile<'a> {
    /// The module `module` is in the file of content `file` at `path`, relative to the crate's
    /// root directory with `/` between its parts (rule 25: never absolute). `inline` is the
    /// `mod name { .. }` item of that file the module is, `None` for the whole file.
    pub fn new_in(
        module: ItemKey,
        file: ContentId,
        path: &str,
        inline: Option<ItemIndex>,
        alloc: Alloc<'a>,
    ) -> Result<ModuleFile<'a>, BuildError> {
        let mut staging = Strings::new_in(alloc);
        let path = staging.push(path, "ModuleFile strings")?;
        Ok(ModuleFile { module, file, path, inline, absent: AVec::new_in(alloc), staging, alloc })
    }

    /// A candidate path read and found absent, relative as the module's path is.
    pub fn absent(&mut self, path: &str) -> Result<(), BuildError> {
        let path = self.staging.push(path, "ModuleFile strings")?;
        self.absent.push(path);
        Ok(())
    }

    pub fn finish(mut self) -> Result<RecordBytes<'a>, BuildError> {
        let staging = &self.staging;
        self.absent.sort_unstable_by(|a, b| staging.of(*a).cmp(staging.of(*b)));
        self.absent.dedup_by(|a, b| staging.of(*a) == staging.of(*b));
        let mut strings = Strings::new_in(self.alloc);
        let path = strings.moved(&self.staging, self.path, "ModuleFile strings")?;
        for text in &mut self.absent {
            *text = strings.moved(&self.staging, *text, "ModuleFile strings")?;
        }
        let mut out =
            Writer::new(WriterOpts { schema: <ModuleFileRoot as blaze_format::Record>::SCHEMA, alloc: self.alloc, capacity: None });
        let text = Column::push(&mut out, strings.bytes());
        let absent = Column::push(&mut out, &self.absent);
        Ok(finish_with(out, |at| ModuleFileRoot {
            module: self.module.0,
            file: self.file,
            path,
            inline: index_word(self.inline),
            absent: absent.slice(at, offset_of!(ModuleFileRoot, absent)),
            strings: text.slice(at, offset_of!(ModuleFileRoot, strings)),
        }))
    }
}

/// A [`ModuleFile`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct ModuleFileView<'a> {
    module: ItemKey,
    file: ContentId,
    path: &'a str,
    inline: Option<ItemIndex>,
    absent: &'a [Text],
    strings: &'a str,
}

impl<'a> ModuleFileView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<ModuleFileView<'a>, FormatError> {
        let root = blaze_format::open::<ModuleFileRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        check_text(strings, root.path, blaze_format::Verified::get(root), buffer)?;
        let absent = root.absent.follow(buffer)?;
        for text in absent {
            check_text(strings, *text, text, buffer)?;
        }
        check_increasing(absent, buffer, "absent paths out of order or repeated", |text| {
            text_of(strings, *text)
        })?;
        Ok(ModuleFileView {
            module: ItemKey(root.module),
            file: root.file,
            path: text_of(strings, root.path),
            inline: index_of(root.inline),
            absent,
            strings,
        })
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    /// The content of the file holding the module's items.
    pub fn file(&self) -> ContentId {
        self.file
    }

    /// The file's path, relative to the crate's root directory.
    pub fn path(&self) -> &'a str {
        self.path
    }

    /// The `mod name { .. }` item of the file the module is; `None` for the whole file.
    pub fn inline(&self) -> Option<ItemIndex> {
        self.inline
    }

    /// The candidate paths read and found absent, sorted.
    pub fn absent(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.absent.iter().map(move |text| text_of(strings, *text))
    }
}

