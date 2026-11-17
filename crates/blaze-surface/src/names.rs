//! The other records name resolution reads (docs/v2/plan/design/bottom.md, "The other records
//! names need"): [`TextualEntry`], [`ModuleFile`], [`CrateRoot`] and [`MacroDef`].

use core::alloc::Allocator;
use core::mem::offset_of;

use blaze_format::{FormatError, Slice, Writer};
use blaze_id::{ContentId, CrateItemKey, ItemKey};

use crate::column::{
    Column, NONE, Strings, Text, check_increasing, check_text, finish_with, index_of, index_word,
    key_of_word, key_word, text_of,
};
use crate::error::BuildError;
use crate::kind::{Edition, ItemIndex, MacroKind, Place, PlaceTag};

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
pub struct TextualEntry<A: Allocator + Clone> {
    module: ItemKey,
    rows: Vec<MacroRow, A>,
    staging: Strings<A>,
    alloc: A,
}

impl<A: Allocator + Clone> TextualEntry<A> {
    pub fn new_in(module: ItemKey, alloc: A) -> TextualEntry<A> {
        TextualEntry {
            module,
            rows: Vec::new_in(alloc.clone()),
            staging: Strings::new_in(alloc.clone()),
            alloc,
        }
    }

    /// The next `macro_rules!` in textual order: its name and its [`MacroDef`]'s item.
    pub fn push(&mut self, name: &str, def: ItemKey) -> Result<(), BuildError> {
        let name = self.staging.push(name, "TextualEntry strings")?;
        self.rows.push(MacroRow { name, def: def.0 });
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let mut strings = Strings::new_in(self.alloc.clone());
        for row in &mut self.rows {
            row.name = strings.moved(&self.staging, row.name, "TextualEntry strings")?;
        }
        let mut out =
            Writer::new_in(<TextualEntryRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
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
pub struct ModuleFile<A: Allocator + Clone> {
    module: ItemKey,
    file: ContentId,
    path: Text,
    inline: Option<ItemIndex>,
    absent: Vec<Text, A>,
    staging: Strings<A>,
    alloc: A,
}

impl<A: Allocator + Clone> ModuleFile<A> {
    /// The module `module` is in the file of content `file` at `path`, relative to the crate's
    /// root directory with `/` between its parts (rule 25: never absolute). `inline` is the
    /// `mod name { .. }` item of that file the module is, `None` for the whole file.
    pub fn new_in(
        module: ItemKey,
        file: ContentId,
        path: &str,
        inline: Option<ItemIndex>,
        alloc: A,
    ) -> Result<ModuleFile<A>, BuildError> {
        let mut staging = Strings::new_in(alloc.clone());
        let path = staging.push(path, "ModuleFile strings")?;
        Ok(ModuleFile { module, file, path, inline, absent: Vec::new_in(alloc.clone()), staging, alloc })
    }

    /// A candidate path read and found absent, relative as the module's path is.
    pub fn absent(&mut self, path: &str) -> Result<(), BuildError> {
        let path = self.staging.push(path, "ModuleFile strings")?;
        self.absent.push(path);
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let staging = &self.staging;
        self.absent.sort_unstable_by(|a, b| staging.of(*a).cmp(staging.of(*b)));
        self.absent.dedup_by(|a, b| staging.of(*a) == staging.of(*b));
        let mut strings = Strings::new_in(self.alloc.clone());
        let path = strings.moved(&self.staging, self.path, "ModuleFile strings")?;
        for text in &mut self.absent {
            *text = strings.moved(&self.staging, *text, "ModuleFile strings")?;
        }
        let mut out =
            Writer::new_in(<ModuleFileRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
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
        check_text(strings, root.path, root, buffer)?;
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

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ExternRow: 1 {
        pub(crate) name: Text,
        pub(crate) krate: ContentId,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct CrateRootRoot: 1 {
        pub(crate) krate: ContentId,
        pub(crate) root_module: ContentId,
        pub(crate) root_file: ContentId,
        pub(crate) prelude: ContentId,
        pub(crate) externs: Slice<ExternRow>,
        pub(crate) self_names: Slice<Text>,
        pub(crate) tools: Slice<Text>,
        pub(crate) strings: Slice<u8>,
        pub(crate) edition: Edition,
        pub(crate) no_std: bool,
        pub(crate) no_core: bool,
        pub(crate) pad: u8,
    }
}

/// What a crate's root record says of the crate itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CrateRootHead {
    pub krate: CrateItemKey,
    /// The crate's root module.
    pub root_module: ItemKey,
    /// The content of the crate's root file.
    pub root_file: ContentId,
    pub edition: Edition,
    /// `#![no_std]`.
    pub no_std: bool,
    /// `#![no_core]`.
    pub no_core: bool,
    /// The prelude module the crate's modules start from (`std::prelude::rust_2024`), `None`
    /// under `#![no_core]` or `#![no_implicit_prelude]`.
    pub prelude: Option<ItemKey>,
}

/// The writer of one crate's root record: the edition, `no_std`, the prelude, the extern
/// prelude (with `extern crate self as name`) and `#![register_tool]`s. Each list is sorted at
/// the end.
pub struct CrateRoot<A: Allocator + Clone> {
    head: CrateRootHead,
    externs: Vec<ExternRow, A>,
    self_names: Vec<Text, A>,
    tools: Vec<Text, A>,
    staging: Strings<A>,
    alloc: A,
}

impl<A: Allocator + Clone> CrateRoot<A> {
    pub fn new_in(head: CrateRootHead, alloc: A) -> CrateRoot<A> {
        CrateRoot {
            head,
            externs: Vec::new_in(alloc.clone()),
            self_names: Vec::new_in(alloc.clone()),
            tools: Vec::new_in(alloc.clone()),
            staging: Strings::new_in(alloc.clone()),
            alloc,
        }
    }

    /// `name` in the extern prelude names the crate `krate`.
    pub fn extern_crate(&mut self, name: &str, krate: CrateItemKey) -> Result<(), BuildError> {
        let name = self.staging.push(name, "CrateRoot strings")?;
        self.externs.push(ExternRow { name, krate: krate.0 });
        Ok(())
    }

    /// `extern crate self as name;`.
    pub fn self_name(&mut self, name: &str) -> Result<(), BuildError> {
        let name = self.staging.push(name, "CrateRoot strings")?;
        self.self_names.push(name);
        Ok(())
    }

    /// `#![register_tool(name)]`.
    pub fn tool(&mut self, name: &str) -> Result<(), BuildError> {
        let name = self.staging.push(name, "CrateRoot strings")?;
        self.tools.push(name);
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let staging = &self.staging;
        self.externs.sort_unstable_by(|a, b| (staging.of(a.name), a.krate).cmp(&(staging.of(b.name), b.krate)));
        self.externs.dedup_by(|a, b| a.krate == b.krate && staging.of(a.name) == staging.of(b.name));
        if let Some(pair) = self.externs.windows(2).find(|pair| staging.of(pair[0].name) == staging.of(pair[1].name)) {
            return Err(BuildError::Twice { table: "CrateRoot externs", key: pair[1].krate });
        }
        for names in [&mut self.self_names, &mut self.tools] {
            names.sort_unstable_by(|a, b| staging.of(*a).cmp(staging.of(*b)));
            names.dedup_by(|a, b| staging.of(*a) == staging.of(*b));
        }
        let mut strings = Strings::new_in(self.alloc.clone());
        for row in &mut self.externs {
            row.name = strings.moved(&self.staging, row.name, "CrateRoot strings")?;
        }
        for names in [&mut self.self_names, &mut self.tools] {
            for text in names.iter_mut() {
                *text = strings.moved(&self.staging, *text, "CrateRoot strings")?;
            }
        }
        let mut out =
            Writer::new_in(<CrateRootRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let text = Column::push(&mut out, strings.bytes());
        let externs = Column::push(&mut out, &self.externs);
        let self_names = Column::push(&mut out, &self.self_names);
        let tools = Column::push(&mut out, &self.tools);
        let head = self.head;
        Ok(finish_with(out, |at| CrateRootRoot {
            krate: head.krate.0,
            root_module: head.root_module.0,
            root_file: head.root_file,
            prelude: key_word(head.prelude),
            externs: externs.slice(at, offset_of!(CrateRootRoot, externs)),
            self_names: self_names.slice(at, offset_of!(CrateRootRoot, self_names)),
            tools: tools.slice(at, offset_of!(CrateRootRoot, tools)),
            strings: text.slice(at, offset_of!(CrateRootRoot, strings)),
            edition: head.edition,
            no_std: head.no_std,
            no_core: head.no_core,
            pad: 0,
        }))
    }
}

/// A [`CrateRoot`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct CrateRootView<'a> {
    head: CrateRootHead,
    externs: &'a [ExternRow],
    self_names: &'a [Text],
    tools: &'a [Text],
    strings: &'a str,
}

impl<'a> CrateRootView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<CrateRootView<'a>, FormatError> {
        let root = blaze_format::open::<CrateRootRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        let externs = root.externs.follow(buffer)?;
        let self_names = root.self_names.follow(buffer)?;
        let tools = root.tools.follow(buffer)?;
        for row in externs {
            check_text(strings, row.name, row, buffer)?;
        }
        for text in self_names.iter().chain(tools) {
            check_text(strings, *text, text, buffer)?;
        }
        check_increasing(externs, buffer, "extern prelude out of order or repeated", |row| {
            text_of(strings, row.name)
        })?;
        for names in [self_names, tools] {
            check_increasing(names, buffer, "names out of order or repeated", |text| text_of(strings, *text))?;
        }
        Ok(CrateRootView {
            head: CrateRootHead {
                krate: CrateItemKey(root.krate),
                root_module: ItemKey(root.root_module),
                root_file: root.root_file,
                edition: root.edition,
                no_std: root.no_std,
                no_core: root.no_core,
                prelude: key_of_word(root.prelude),
            },
            externs,
            self_names,
            tools,
            strings,
        })
    }

    pub fn head(&self) -> CrateRootHead {
        self.head
    }

    /// The crate `name` names in the extern prelude, by one binary search.
    pub fn extern_crate(&self, name: &str) -> Option<CrateItemKey> {
        let strings = self.strings;
        let at = self.externs.binary_search_by(|row| text_of(strings, row.name).cmp(name)).ok()?;
        Some(CrateItemKey(self.externs[at].krate))
    }

    /// The extern prelude, by name.
    pub fn externs(&self) -> impl Iterator<Item = (&'a str, CrateItemKey)> + use<'a> {
        let strings = self.strings;
        self.externs.iter().map(move |row| (text_of(strings, row.name), CrateItemKey(row.krate)))
    }

    /// The names of `extern crate self as name;`, sorted.
    pub fn self_names(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.self_names.iter().map(move |text| text_of(strings, *text))
    }

    /// The `#![register_tool]`s, sorted.
    pub fn tools(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.tools.iter().map(move |text| text_of(strings, *text))
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct MacroDefRoot: 1 {
        pub(crate) item: ContentId,
        pub(crate) body: ContentId,
        pub(crate) origin: ContentId,
        pub(crate) name: Text,
        pub(crate) index: u32,
        pub(crate) helpers: Slice<Text>,
        pub(crate) strings: Slice<u8>,
        pub(crate) kind: MacroKind,
        pub(crate) place: PlaceTag,
        pub(crate) placed: bool,
        pub(crate) exported: bool,
        pub(crate) local_inner: bool,
        pub(crate) pad: [u8; 3],
    }
}

/// What a macro definition is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroDefHead<'a> {
    /// The macro's item.
    pub item: ItemKey,
    pub name: &'a str,
    /// The digest of the definition's tokens: what an `Expansion` is keyed by (the
    /// definition's content, not its item key, so a moved macro keeps its expansions).
    pub body: ContentId,
    /// Where it is written; `None` for a builtin macro.
    pub place: Option<Place>,
    pub kind: MacroKind,
    /// `#[macro_export]`.
    pub exported: bool,
    /// `#[macro_export(local_inner_macros)]`.
    pub local_inner: bool,
}

/// The writer of one macro's [`MacroDef`]: its head and, for a derive, its helper attributes in
/// the order `attributes(..)` lists them (the source's order, which fixes the bytes).
pub struct MacroDef<'a, A: Allocator + Clone> {
    head: MacroDefHead<'a>,
    helpers: Vec<Text, A>,
    staging: Strings<A>,
    alloc: A,
}

impl<'a, A: Allocator + Clone> MacroDef<'a, A> {
    pub fn new_in(head: MacroDefHead<'a>, alloc: A) -> MacroDef<'a, A> {
        MacroDef {
            head,
            helpers: Vec::new_in(alloc.clone()),
            staging: Strings::new_in(alloc.clone()),
            alloc,
        }
    }

    /// A derive's helper attribute.
    pub fn helper(&mut self, name: &str) -> Result<(), BuildError> {
        let name = self.staging.push(name, "MacroDef strings")?;
        self.helpers.push(name);
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let mut strings = Strings::new_in(self.alloc.clone());
        let name = strings.push(self.head.name, "MacroDef strings")?;
        for text in &mut self.helpers {
            *text = strings.moved(&self.staging, *text, "MacroDef strings")?;
        }
        let mut out = Writer::new_in(<MacroDefRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let text = Column::push(&mut out, strings.bytes());
        let helpers = Column::push(&mut out, &self.helpers);
        let head = self.head;
        let (place, origin, index) = match head.place {
            Some(place) => place.to_record(),
            None => (PlaceTag::File, ContentId([0; 32]), NONE),
        };
        Ok(finish_with(out, |at| MacroDefRoot {
            item: head.item.0,
            body: head.body,
            origin,
            name,
            index,
            helpers: helpers.slice(at, offset_of!(MacroDefRoot, helpers)),
            strings: text.slice(at, offset_of!(MacroDefRoot, strings)),
            kind: head.kind,
            place,
            placed: head.place.is_some(),
            exported: head.exported,
            local_inner: head.local_inner,
            pad: [0; 3],
        }))
    }
}

/// A [`MacroDef`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct MacroDefView<'a> {
    head: MacroDefHead<'a>,
    helpers: &'a [Text],
    strings: &'a str,
}

impl<'a> MacroDefView<'a> {
    pub fn read(buffer: &'a [u8]) -> Result<MacroDefView<'a>, FormatError> {
        let root = blaze_format::open::<MacroDefRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        check_text(strings, root.name, root, buffer)?;
        let helpers = root.helpers.follow(buffer)?;
        for text in helpers {
            check_text(strings, *text, text, buffer)?;
        }
        Ok(MacroDefView {
            head: MacroDefHead {
                item: ItemKey(root.item),
                name: text_of(strings, root.name),
                body: root.body,
                place: root.placed.then(|| Place::from_record(root.place, root.origin, root.index)),
                kind: root.kind,
                exported: root.exported,
                local_inner: root.local_inner,
            },
            helpers,
            strings,
        })
    }

    pub fn head(&self) -> MacroDefHead<'a> {
        self.head
    }

    /// A derive's helper attributes, in the order it lists them.
    pub fn helpers(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.helpers.iter().map(move |text| text_of(strings, *text))
    }
}
