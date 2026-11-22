//! [`ModuleDecls`]: what a module declares itself, before imports and macros, keyed by the
//! module's `FileItems`, the outcomes of the cfg predicates it mentions and the edition; and
//! [`DeclsBuilder`], the same declarations as a working form, which is also what one macro
//! call's expansion adds ([`ExpandedDecls`]).
//!
//! The record holds the module's place too ([`ModuleInfo`]: its crate, its parent, its name in
//! the parent, `#[no_implicit_prelude]`), all read from the parent's `mod` line and the
//! module's own attributes, so a name resolver reads one record per module of its chain.
//!
//! Every list is in source order, which the record's key fixes: the order is meaningful (the
//! first of two declarations of one name is kept, a `use`'s index names it). The string table
//! and the path columns are laid out in the record's own order as it is written.

use core::fmt;
use core::mem::offset_of;

use blaze_format::{FormatError, Record, Slice, Writer, WriterOpts};
use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_own::{AVec, Alloc, Vec};

use crate::column::{
    Column, NONE, RecordBytes, Strings, Text, at_of, at_of_record, check_text, finish_with,
    key_of_word, key_word, text_of,
};
use crate::error::BuildError;
use crate::kind::{
    CallRole, CallShape, Def, DefKind, DefKindTag, MacroShapes, Namespace, WrittenVis, WrittenVisTag,
    byte_enum,
};
use crate::path::{
    ExternTarget, Path, PathRow, PathTable, PathView, Paths, Rename, RenameTag, UseForm,
};

/// An item the module declares itself, in one namespace (a tuple struct is two of these). `P`
/// is a path: [`Path`] in a working form, [`PathView`] read from a record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DeclaredItem<N, P> {
    pub name: N,
    pub ns: Namespace,
    pub def: Def,
    pub vis: WrittenVis<P>,
}

/// A `mod` child. `#[path]` and the `foo.rs` / `foo/mod.rs` choice are already decided: the
/// child is named by its key, and which file it is in is the `ModuleFile` record's question.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ChildModDecl<N, P> {
    pub name: N,
    pub module: ItemKey,
    pub vis: WrittenVis<P>,
    /// `#[macro_use] mod child;`: the child's `macro_rules!` stay visible after this line.
    pub macro_use: bool,
    /// Its place in the fragment's source order (unique among the fragment's ordered entries).
    pub pos: u32,
}

/// One leaf of a `use` tree: `use a::{b, c::*}` is two.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UseDecl<N, P> {
    pub path: P,
    pub leaf: UseForm<N>,
    pub vis: WrittenVis<P>,
    pub pos: u32,
}

/// An `extern crate` item. What it adds to the extern prelude and the `#[macro_use]` prelude
/// at the crate root is in the crate's root record; here it is only the binding in its own
/// module.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExternCrateDecl<N, P> {
    pub target: ExternTarget<N>,
    pub rename: Rename<N>,
    pub vis: WrittenVis<P>,
}

/// A `macro_rules!` definition, in the fragment's source order. A `#[macro_export]` one
/// written in source is in the crate root's scope through the root record's macro exports;
/// one a macro expansion writes is listed in its module's scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MacroRulesDecl<N> {
    pub name: N,
    pub def: ItemKey,
    pub pos: u32,
    /// `#[macro_export]`.
    pub export: bool,
    /// The call shapes its arms are written for.
    pub shapes: MacroShapes,
}

/// An item-position macro call: a function-like call, an attribute macro on an item (the item
/// is not a declared name until the call expands), or a derive.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MacroCallDecl<P> {
    /// The call's identity: the key its expansion's items are keyed under.
    pub call: ItemKey,
    pub path: P,
    pub kind: CallShape,
    pub pos: u32,
    /// The item an attribute or a derive is on, an index unique in the fragment: a derive's
    /// helper attributes are in scope for the attributes written after it on the same item.
    pub item: Option<u32>,
    /// What the call is to its item ([`CallRole`]); `Call` for every call not on a derived item.
    pub role: CallRole,
}

/// A module's place in its crate, read from its parent's `mod` line and its own attributes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ModuleInfo<N> {
    pub krate: CrateItemKey,
    /// `None` for a crate root.
    pub parent: Option<ItemKey>,
    /// Its name in its parent (`None` for a crate root).
    pub name: Option<N>,
    /// `#[no_implicit_prelude]` on this module (it holds for the modules under it too).
    pub no_implicit_prelude: bool,
}

// SAFETY: keys and a flag; a name is `Portable` when `N` is.
unsafe impl<N: Portable> Portable for ModuleInfo<N> {}

/// What one fragment of a module declares, as a working form: the module's own text (with
/// `cfg` already decided: an item `cfg`'d out is not here), or one macro call's expansion.
/// [`DeclsBuilder::write`] writes it as a [`ModuleDecls`] record and [`DeclsBuilder::from`]
/// reads it back, equal.
#[derive(Clone, Debug)]
pub struct DeclsBuilder<'a, N> {
    pub items: AVec<'a, DeclaredItem<N, Path<'a, N>>>,
    pub mods: AVec<'a, ChildModDecl<N, Path<'a, N>>>,
    pub uses: AVec<'a, UseDecl<N, Path<'a, N>>>,
    pub extern_crates: AVec<'a, ExternCrateDecl<N, Path<'a, N>>>,
    pub macro_rules: AVec<'a, MacroRulesDecl<N>>,
    pub calls: AVec<'a, MacroCallDecl<Path<'a, N>>>,
}

/// What a macro call's expansion adds to its module: the same shape as the module's own text.
pub type ExpandedDecls<'a, N> = DeclsBuilder<'a, N>;

impl<'b, N: PartialEq> PartialEq<DeclsBuilder<'b, N>> for DeclsBuilder<'_, N> {
    fn eq(&self, other: &DeclsBuilder<'b, N>) -> bool {
        fn same<T, U>(a: &[T], b: &[U], eq: impl Fn(&T, &U) -> bool) -> bool {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| eq(x, y))
        }
        fn vis<N: PartialEq>(a: &WrittenVis<Path<'_, N>>, b: &WrittenVis<Path<'_, N>>) -> bool {
            match (a.as_ref().split(), b.as_ref().split()) {
                ((x, Some(p)), (y, Some(q))) => x == y && p == q,
                ((x, None), (y, None)) => x == y,
                _ => false,
            }
        }
        same(&self.items, &other.items, |a, b| a.name == b.name && a.ns == b.ns && a.def == b.def && vis(&a.vis, &b.vis))
            && same(&self.mods, &other.mods, |a, b| {
                a.name == b.name && a.module == b.module && vis(&a.vis, &b.vis) && a.macro_use == b.macro_use && a.pos == b.pos
            })
            && same(&self.uses, &other.uses, |a, b| a.path == b.path && a.leaf == b.leaf && vis(&a.vis, &b.vis) && a.pos == b.pos)
            && same(&self.extern_crates, &other.extern_crates, |a, b| {
                a.target == b.target && a.rename == b.rename && vis(&a.vis, &b.vis)
            })
            && self.macro_rules[..] == other.macro_rules[..]
            && same(&self.calls, &other.calls, |a, b| {
                a.call == b.call && a.path == b.path && a.kind == b.kind && a.pos == b.pos && a.item == b.item
            })
    }
}

impl<'a, N> DeclsBuilder<'a, N> {
    /// Nothing declared.
    pub fn new_in(alloc: Alloc<'a>) -> DeclsBuilder<'a, N> {
        DeclsBuilder {
            items: AVec::new_in(alloc),
            mods: AVec::new_in(alloc),
            uses: AVec::new_in(alloc),
            extern_crates: AVec::new_in(alloc),
            macro_rules: AVec::new_in(alloc),
            calls: AVec::new_in(alloc),
        }
    }
}

byte_enum! {
    /// [`UseForm`]'s byte in a record.
    pub(crate) enum UseTag, refused as "a byte that is no use form" {
        Named = 0 => "named",
        Glob = 1 => "glob",
    }
}

byte_enum! {
    /// [`ExternTarget`]'s byte in a record.
    pub(crate) enum ExternTargetTag, refused as "a byte that is no extern crate target" {
        Dep = 0 => "dependency",
        SelfCrate = 1 => "self",
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ItemRow: 1 {
        pub(crate) name: Text,
        pub(crate) vis_path: u32,
        pub(crate) key: ContentId,
        pub(crate) ns: Namespace,
        pub(crate) kind: DefKindTag,
        pub(crate) shapes: MacroShapes,
        pub(crate) vis: WrittenVisTag,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ModRow: 1 {
        pub(crate) name: Text,
        pub(crate) vis_path: u32,
        pub(crate) pos: u32,
        pub(crate) module: ContentId,
        pub(crate) vis: WrittenVisTag,
        pub(crate) macro_use: bool,
        pub(crate) pad: u16,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct UseRow: 1 {
        pub(crate) rename: Text,
        pub(crate) path: u32,
        pub(crate) vis_path: u32,
        pub(crate) pos: u32,
        pub(crate) form: UseTag,
        pub(crate) rename_tag: RenameTag,
        pub(crate) types_only: bool,
        pub(crate) vis: WrittenVisTag,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ExternCrateRow: 1 {
        pub(crate) target: Text,
        pub(crate) rename: Text,
        pub(crate) vis_path: u32,
        pub(crate) target_tag: ExternTargetTag,
        pub(crate) rename_tag: RenameTag,
        pub(crate) vis: WrittenVisTag,
        pub(crate) pad: u8,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct RulesRow: 1 {
        pub(crate) name: Text,
        pub(crate) pos: u32,
        pub(crate) def: ContentId,
        pub(crate) export: bool,
        pub(crate) shapes: MacroShapes,
        pub(crate) pad2: u16,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct CallRow: 2 {
        pub(crate) path: u32,
        pub(crate) pos: u32,
        pub(crate) item: u32,
        pub(crate) call: ContentId,
        pub(crate) kind: CallShape,
        pub(crate) role: CallRole,
        pub(crate) pad2: u16,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ModuleDeclsRoot: 1 {
        pub(crate) module: ContentId,
        pub(crate) krate: ContentId,
        pub(crate) parent: ContentId,
        pub(crate) name: Text,
        pub(crate) items: Slice<ItemRow>,
        pub(crate) mods: Slice<ModRow>,
        pub(crate) uses: Slice<UseRow>,
        pub(crate) externs: Slice<ExternCrateRow>,
        pub(crate) rules: Slice<RulesRow>,
        pub(crate) calls: Slice<CallRow>,
        pub(crate) paths: Slice<PathRow>,
        pub(crate) segments: Slice<Text>,
        pub(crate) strings: Slice<u8>,
        pub(crate) has_name: bool,
        pub(crate) no_implicit_prelude: bool,
        pub(crate) pad: u16,
    }
}

const TABLE: &str = "ModuleDecls";

/// No text: what a row holds where its value has none, so two builders write equal bytes.
pub(crate) const NO_TEXT: Text = Text { at: 0, len: 0 };

/// A written visibility as a row holds it: its byte and its path's row, [`NONE`] for none.
pub(crate) fn vis_row<N: AsRef<str>>(
    vis: &WrittenVis<Path<'_, N>>,
    paths: &mut PathTable<'_>,
    strings: &mut Strings<'_>,
    table: &'static str,
) -> Result<(WrittenVisTag, u32), BuildError> {
    let (tag, path) = vis.as_ref().split();
    Ok((tag, paths.push_opt(path, strings, table)?))
}

/// Whether a row's visibility names a path exactly when its byte has one.
pub(crate) fn check_vis<T: Record>(
    paths: &Paths<'_>,
    tag: WrittenVisTag,
    index: u32,
    row: &T,
    buffer: &[u8],
) -> Result<(), FormatError> {
    match tag {
        WrittenVisTag::In | WrittenVisTag::WithoutIn => paths.check(index, false, row, buffer),
        WrittenVisTag::Private
        | WrittenVisTag::Pub
        | WrittenVisTag::Crate
        | WrittenVisTag::Super
        | WrittenVisTag::SelfOnly => match index == NONE {
            true => Ok(()),
            false => Err(FormatError::Invalid {
                at: at_of_record(row, buffer),
                what: "a visibility with no path that names one",
            }),
        },
    }
}

/// [`vis_row`] read back.
pub(crate) fn vis_of<'a>(paths: &Paths<'a>, tag: WrittenVisTag, index: u32) -> WrittenVis<PathView<'a>> {
    WrittenVis::join(tag, || paths.get(index))
}

fn text_or_none<N: AsRef<str>>(name: Option<&N>, strings: &mut Strings<'_>) -> Result<Text, BuildError> {
    match name {
        Some(name) => strings.push(name.as_ref(), TABLE),
        None => Ok(NO_TEXT),
    }
}

impl<N: AsRef<str>> DeclsBuilder<'_, N> {
    /// The [`ModuleDecls`] record of `module`, whose place is `info`, in `alloc`.
    pub fn write<'b>(
        &self,
        module: ItemKey,
        info: &ModuleInfo<N>,
        alloc: Alloc<'b>,
    ) -> Result<RecordBytes<'b>, BuildError> {
        let mut strings = Strings::new_in(alloc);
        let mut paths = PathTable::new_in(alloc);
        let name = text_or_none(info.name.as_ref(), &mut strings)?;

        let mut items = Vec::with_capacity_in(self.items.len(), alloc);
        for it in self.items.iter() {
            let name = strings.push(it.name.as_ref(), TABLE)?;
            let (vis, vis_path) = vis_row(&it.vis, &mut paths, &mut strings, TABLE)?;
            let (kind, shapes) = it.def.kind.to_record();
            items.push(ItemRow { name, vis_path, key: it.def.key.0, ns: it.ns, kind, shapes, vis });
        }
        let mut mods = Vec::with_capacity_in(self.mods.len(), alloc);
        for m in self.mods.iter() {
            let name = strings.push(m.name.as_ref(), TABLE)?;
            let (vis, vis_path) = vis_row(&m.vis, &mut paths, &mut strings, TABLE)?;
            mods.push(ModRow { name, vis_path, pos: m.pos, module: m.module.0, vis, macro_use: m.macro_use, pad: 0 });
        }
        let mut uses = Vec::with_capacity_in(self.uses.len(), alloc);
        for u in self.uses.iter() {
            let path = paths.push(&u.path, &mut strings, TABLE)?;
            let (form, rename_tag, rename, types_only) = match u.leaf {
                UseForm::Glob => (UseTag::Glob, RenameTag::Same, NO_TEXT, false),
                UseForm::Named { ref rename, types_only } => {
                    let (tag, name) = rename.as_ref().split();
                    (UseTag::Named, tag, text_or_none(name, &mut strings)?, types_only)
                }
            };
            let (vis, vis_path) = vis_row(&u.vis, &mut paths, &mut strings, TABLE)?;
            uses.push(UseRow { rename, path, vis_path, pos: u.pos, form, rename_tag, types_only, vis });
        }
        let mut externs = Vec::with_capacity_in(self.extern_crates.len(), alloc);
        for ec in self.extern_crates.iter() {
            let (target_tag, target) = match &ec.target {
                ExternTarget::Dep(name) => (ExternTargetTag::Dep, strings.push(name.as_ref(), TABLE)?),
                ExternTarget::SelfCrate => (ExternTargetTag::SelfCrate, NO_TEXT),
            };
            let (rename_tag, name) = ec.rename.as_ref().split();
            let rename = text_or_none(name, &mut strings)?;
            let (vis, vis_path) = vis_row(&ec.vis, &mut paths, &mut strings, TABLE)?;
            externs.push(ExternCrateRow { target, rename, vis_path, target_tag, rename_tag, vis, pad: 0 });
        }
        let mut rules = Vec::with_capacity_in(self.macro_rules.len(), alloc);
        for mr in self.macro_rules.iter() {
            let name = strings.push(mr.name.as_ref(), TABLE)?;
            rules.push(RulesRow { name, pos: mr.pos, def: mr.def.0, export: mr.export, shapes: mr.shapes, pad2: 0 });
        }
        let mut calls = Vec::with_capacity_in(self.calls.len(), alloc);
        for c in self.calls.iter() {
            let path = paths.push(&c.path, &mut strings, TABLE)?;
            let item = match c.item {
                Some(NONE) => return Err(BuildError::BadIndex { table: "ModuleDecls calls", index: NONE }),
                Some(item) => item,
                None => NONE,
            };
            calls.push(CallRow { path, pos: c.pos, item, call: c.call.0, kind: c.kind, role: c.role, pad2: 0 });
        }

        let mut out = Writer::new(WriterOpts { schema: <ModuleDeclsRoot as Record>::SCHEMA, alloc: alloc, capacity: None });
        let text = Column::push(&mut out, strings.bytes());
        let items = Column::push(&mut out, &items);
        let mods = Column::push(&mut out, &mods);
        let uses = Column::push(&mut out, &uses);
        let externs = Column::push(&mut out, &externs);
        let rules = Column::push(&mut out, &rules);
        let calls = Column::push(&mut out, &calls);
        let path_rows = Column::push(&mut out, &paths.rows);
        let segments = Column::push(&mut out, &paths.segments);
        Ok(finish_with(out, |at| ModuleDeclsRoot {
            module: module.0,
            krate: info.krate.0,
            parent: key_word(info.parent),
            name,
            items: items.slice(at, offset_of!(ModuleDeclsRoot, items)),
            mods: mods.slice(at, offset_of!(ModuleDeclsRoot, mods)),
            uses: uses.slice(at, offset_of!(ModuleDeclsRoot, uses)),
            externs: externs.slice(at, offset_of!(ModuleDeclsRoot, externs)),
            rules: rules.slice(at, offset_of!(ModuleDeclsRoot, rules)),
            calls: calls.slice(at, offset_of!(ModuleDeclsRoot, calls)),
            paths: path_rows.slice(at, offset_of!(ModuleDeclsRoot, paths)),
            segments: segments.slice(at, offset_of!(ModuleDeclsRoot, segments)),
            strings: text.slice(at, offset_of!(ModuleDeclsRoot, strings)),
            has_name: info.name.is_some(),
            no_implicit_prelude: info.no_implicit_prelude,
            pad: 0,
        }))
    }
}

impl<'a, 'b> DeclsBuilder<'b, &'a str> {
    /// The working form of a [`ModuleDecls`] record, its names borrowed from the record, in
    /// `alloc`: equal to the builder that wrote it.
    pub fn from(view: &ModuleDeclsView<'a>, alloc: Alloc<'b>) -> DeclsBuilder<'b, &'a str> {
        let path = |p: PathView<'a>| p.to_path_in(alloc);
        let mut out = DeclsBuilder::new_in(alloc);
        out.items.extend(view.items().map(|it| DeclaredItem { name: it.name, ns: it.ns, def: it.def, vis: it.vis.map(path) }));
        out.mods.extend(view.mods().map(|m| ChildModDecl {
            name: m.name,
            module: m.module,
            vis: m.vis.map(path),
            macro_use: m.macro_use,
            pos: m.pos,
        }));
        out.uses.extend(view.uses().map(|u| UseDecl { path: path(u.path), leaf: u.leaf, vis: u.vis.map(path), pos: u.pos }));
        out.extern_crates.extend(view.extern_crates().map(|ec| ExternCrateDecl {
            target: ec.target,
            rename: ec.rename,
            vis: ec.vis.map(path),
        }));
        out.macro_rules.extend(view.macro_rules());
        out.calls.extend(view.calls().map(|c| MacroCallDecl {
            call: c.call,
            path: path(c.path),
            kind: c.kind,
            pos: c.pos,
            item: c.item,
            role: c.role,
        }));
        out
    }
}

/// A [`ModuleDecls`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct ModuleDeclsView<'a> {
    module: ItemKey,
    info: ModuleInfo<&'a str>,
    items: &'a [ItemRow],
    mods: &'a [ModRow],
    uses: &'a [UseRow],
    externs: &'a [ExternCrateRow],
    rules: &'a [RulesRow],
    calls: &'a [CallRow],
    paths: Paths<'a>,
    strings: &'a str,
}

impl<'a> ModuleDeclsView<'a> {
    /// The record in `buffer`, checked once: its layout, every name, every path a row names.
    pub fn read(buffer: &'a [u8]) -> Result<ModuleDeclsView<'a>, FormatError> {
        let root = blaze_format::open::<ModuleDeclsRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        let paths = Paths::read(root.paths.follow(buffer)?, root.segments.follow(buffer)?, strings, buffer)?;
        check_text(strings, root.name, blaze_format::Verified::get(root), buffer)?;
        let items = root.items.follow(buffer)?;
        for row in items {
            check_text(strings, row.name, row, buffer)?;
            check_vis(&paths, row.vis, row.vis_path, row, buffer)?;
        }
        let mods = root.mods.follow(buffer)?;
        for row in mods {
            check_text(strings, row.name, row, buffer)?;
            check_vis(&paths, row.vis, row.vis_path, row, buffer)?;
        }
        let uses = root.uses.follow(buffer)?;
        for row in uses {
            check_text(strings, row.rename, row, buffer)?;
            paths.check(row.path, false, row, buffer)?;
            check_vis(&paths, row.vis, row.vis_path, row, buffer)?;
        }
        let externs = root.externs.follow(buffer)?;
        for row in externs {
            check_text(strings, row.target, row, buffer)?;
            check_text(strings, row.rename, row, buffer)?;
            check_vis(&paths, row.vis, row.vis_path, row, buffer)?;
        }
        let rules = root.rules.follow(buffer)?;
        for row in rules {
            check_text(strings, row.name, row, buffer)?;
        }
        let calls = root.calls.follow(buffer)?;
        for row in calls {
            paths.check(row.path, false, row, buffer)?;
        }
        Ok(ModuleDeclsView {
            module: ItemKey(root.module),
            info: ModuleInfo {
                krate: CrateItemKey(root.krate),
                parent: key_of_word(root.parent),
                name: root.has_name.then(|| text_of(strings, root.name)),
                no_implicit_prelude: root.no_implicit_prelude,
            },
            items,
            mods,
            uses,
            externs,
            rules,
            calls,
            paths,
            strings,
        })
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    /// The module's place in its crate.
    pub fn info(&self) -> ModuleInfo<&'a str> {
        self.info
    }

    /// The items declared, in source order.
    pub fn items(&self) -> impl ExactSizeIterator<Item = DeclaredItem<&'a str, PathView<'a>>> + use<'a> {
        let (strings, paths) = (self.strings, self.paths);
        self.items.iter().map(move |row| DeclaredItem {
            name: text_of(strings, row.name),
            ns: row.ns,
            def: Def { key: ItemKey(row.key), kind: DefKind::from_record(row.kind, row.shapes) },
            vis: vis_of(&paths, row.vis, row.vis_path),
        })
    }

    /// The `mod` children, in source order.
    pub fn mods(&self) -> impl ExactSizeIterator<Item = ChildModDecl<&'a str, PathView<'a>>> + use<'a> {
        let (strings, paths) = (self.strings, self.paths);
        self.mods.iter().map(move |row| ChildModDecl {
            name: text_of(strings, row.name),
            module: ItemKey(row.module),
            vis: vis_of(&paths, row.vis, row.vis_path),
            macro_use: row.macro_use,
            pos: row.pos,
        })
    }

    /// The `use` leaves, in source order: a leaf's index here is its `UseSite::index`.
    pub fn uses(&self) -> impl ExactSizeIterator<Item = UseDecl<&'a str, PathView<'a>>> + use<'a> {
        let (strings, paths) = (self.strings, self.paths);
        self.uses.iter().map(move |row| UseDecl {
            path: paths.get(row.path),
            leaf: match row.form {
                UseTag::Glob => UseForm::Glob,
                UseTag::Named => UseForm::Named {
                    rename: Rename::join(row.rename_tag, || text_of(strings, row.rename)),
                    types_only: row.types_only,
                },
            },
            vis: vis_of(&paths, row.vis, row.vis_path),
            pos: row.pos,
        })
    }

    /// The `extern crate` items, in source order.
    pub fn extern_crates(&self) -> impl ExactSizeIterator<Item = ExternCrateDecl<&'a str, PathView<'a>>> + use<'a> {
        let (strings, paths) = (self.strings, self.paths);
        self.externs.iter().map(move |row| ExternCrateDecl {
            target: match row.target_tag {
                ExternTargetTag::Dep => ExternTarget::Dep(text_of(strings, row.target)),
                ExternTargetTag::SelfCrate => ExternTarget::SelfCrate,
            },
            rename: Rename::join(row.rename_tag, || text_of(strings, row.rename)),
            vis: vis_of(&paths, row.vis, row.vis_path),
        })
    }

    /// The `macro_rules!` definitions, in source order.
    pub fn macro_rules(&self) -> impl ExactSizeIterator<Item = MacroRulesDecl<&'a str>> + use<'a> {
        let strings = self.strings;
        self.rules.iter().map(move |row| MacroRulesDecl {
            name: text_of(strings, row.name),
            def: ItemKey(row.def),
            pos: row.pos,
            export: row.export,
            shapes: row.shapes,
        })
    }

    /// The item-position macro calls, in source order.
    pub fn calls(&self) -> impl ExactSizeIterator<Item = MacroCallDecl<PathView<'a>>> + use<'a> {
        let paths = self.paths;
        self.calls.iter().map(move |row| MacroCallDecl {
            call: ItemKey(row.call),
            path: paths.get(row.path),
            kind: row.kind,
            pos: row.pos,
            item: (row.item != NONE).then_some(row.item),
            role: row.role,
        })
    }
}
