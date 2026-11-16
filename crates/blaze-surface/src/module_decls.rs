//! [`ModuleDecls`]: the names a module declares itself, before imports and macros, keyed by the
//! module's `FileItems`, the outcomes of the cfg predicates it mentions and the edition.

use core::alloc::Allocator;
use core::mem::offset_of;

use blaze_format::{FormatError, Slice, Writer};
use blaze_id::{ContentId, ItemKey};

use crate::column::{Column, Strings, Text, check_increasing, check_text, finish_with, text_of};
use crate::error::BuildError;
use crate::kind::{ItemKind, Namespace, Visibility, VisibilityTag};

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct DeclRow: 1 {
        pub(crate) name: Text,
        pub(crate) item: ContentId,
        pub(crate) restricted: ContentId,
        pub(crate) ns: Namespace,
        pub(crate) kind: ItemKind,
        pub(crate) vis: VisibilityTag,
        pub(crate) pad: u8,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ModuleDeclsRoot: 1 {
        pub(crate) module: ContentId,
        pub(crate) names: Slice<DeclRow>,
        pub(crate) strings: Slice<u8>,
    }
}

/// One name a module declares: an item written in it, in one namespace. A unit or tuple struct
/// is two. An item under an attribute macro is not a declared name until it expands: it is in
/// the module's scope with [`crate::Origin::Expanded`], never here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeclaredName<'a> {
    pub ns: Namespace,
    pub name: &'a str,
    pub item: ItemKey,
    pub kind: ItemKind,
    pub vis: Visibility,
}

/// The writer of a module's [`ModuleDecls`]. Names are sorted by namespace, name and item at
/// the end, and an exact repeat is kept once, so the order they are given in moves no byte.
pub struct ModuleDecls<A: Allocator + Clone> {
    module: ItemKey,
    rows: Vec<DeclRow, A>,
    staging: Strings<A>,
    alloc: A,
}

impl<A: Allocator + Clone> ModuleDecls<A> {
    pub fn new_in(module: ItemKey, alloc: A) -> ModuleDecls<A> {
        ModuleDecls {
            module,
            rows: Vec::new_in(alloc.clone()),
            staging: Strings::new_in(alloc.clone()),
            alloc,
        }
    }

    pub fn declare(&mut self, name: DeclaredName<'_>) -> Result<(), BuildError> {
        let text = self.staging.push(name.name, "ModuleDecls strings")?;
        let (vis, restricted) = name.vis.to_record();
        self.rows.push(DeclRow {
            name: text,
            item: name.item.0,
            restricted,
            ns: name.ns,
            kind: name.kind,
            vis,
            pad: 0,
        });
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let staging = &self.staging;
        self.rows.sort_unstable_by(|a, b| {
            (a.ns, staging.of(a.name), a.item, a.kind, a.vis, a.restricted).cmp(&(
                b.ns,
                staging.of(b.name),
                b.item,
                b.kind,
                b.vis,
                b.restricted,
            ))
        });
        self.rows.dedup_by(|a, b| {
            (a.ns, a.item, a.kind, a.vis, a.restricted) == (b.ns, b.item, b.kind, b.vis, b.restricted)
                && staging.of(a.name) == staging.of(b.name)
        });
        let mut strings = Strings::new_in(self.alloc.clone());
        for row in &mut self.rows {
            row.name = strings.moved(&self.staging, row.name, "ModuleDecls strings")?;
        }
        let mut out =
            Writer::new_in(<ModuleDeclsRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let text = Column::push(&mut out, strings.bytes());
        let names = Column::push(&mut out, &self.rows);
        Ok(finish_with(out, |at| ModuleDeclsRoot {
            module: self.module.0,
            names: names.slice(at, offset_of!(ModuleDeclsRoot, names)),
            strings: text.slice(at, offset_of!(ModuleDeclsRoot, strings)),
        }))
    }
}

/// A [`ModuleDecls`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct ModuleDeclsView<'a> {
    module: ItemKey,
    rows: &'a [DeclRow],
    strings: &'a str,
}

impl<'a> ModuleDeclsView<'a> {
    /// The record in `buffer`, checked once: its layout, every name, and its order.
    pub fn read(buffer: &'a [u8]) -> Result<ModuleDeclsView<'a>, FormatError> {
        let root = blaze_format::open::<ModuleDeclsRoot>(buffer)?;
        let view = ModuleDeclsView {
            module: ItemKey(root.module),
            rows: root.names.follow(buffer)?,
            strings: root.strings.as_str(buffer)?,
        };
        for row in view.rows {
            check_text(view.strings, row.name, row, buffer)?;
        }
        check_increasing(view.rows, buffer, "declared names out of order or repeated", |row| {
            (row.ns, text_of(view.strings, row.name), row.item, row.kind, row.vis, row.restricted)
        })?;
        Ok(view)
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    fn decode(&self, row: &DeclRow) -> DeclaredName<'a> {
        DeclaredName {
            ns: row.ns,
            name: text_of(self.strings, row.name),
            item: ItemKey(row.item),
            kind: row.kind,
            vis: Visibility::from_record(row.vis, row.restricted),
        }
    }

    /// Every declared name, by namespace, name and item.
    pub fn names(&self) -> impl Iterator<Item = DeclaredName<'a>> + use<'a> {
        let view = *self;
        self.rows.iter().map(move |row| view.decode(row))
    }

    /// The items declared under `name` in `ns`: one, or more when the module declares a name
    /// twice (an error the reader reports).
    pub fn named(&self, ns: Namespace, name: &str) -> impl Iterator<Item = DeclaredName<'a>> + use<'a> {
        let strings = self.strings;
        let start = self.rows.partition_point(|row| (row.ns, text_of(strings, row.name)) < (ns, name));
        let end = self.rows.partition_point(|row| (row.ns, text_of(strings, row.name)) <= (ns, name));
        let view = *self;
        self.rows[start..end].iter().map(move |row| view.decode(row))
    }
}
