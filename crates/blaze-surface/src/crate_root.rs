//! [`CrateRoot`]: what every module of a crate reads of its crate (docs/v2/plan/design/bottom.md,
//! "The other records names need"); and [`CrateRootBuilder`], the same as a working form.
//!
//! It holds the crate's head ([`CrateRootHead`]: its key, root module and file, edition,
//! `no_std`, `no_core`, prelude), the extern prelude, the dependencies by name, the
//! `#[macro_use]` prelude, the `#[macro_export]` macros written in source, the tool names and
//! the toolchain's built-in attributes. Every list but the `#[macro_use]` prelude, whose order
//! is the source's, is sorted when it is written, so the order it was given in moves no byte.

use core::mem::offset_of;

use blaze_format::{FormatError, Record, Slice, Writer, WriterOpts};
use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use blaze_own::{AVec, Alloc, Vec};

use crate::column::{
    Column, RecordBytes, Strings, Text, check_increasing, check_text, finish_with, key_of_word,
    key_word, text_of,
};
use crate::error::BuildError;
use crate::kind::{Edition, MacroShapes};

mod in_place;

/// What a crate's root record says of the crate itself.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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
    /// `#![blaze::arbitrary_ints]`: `u1`..`u127` and `i1`..`i127` are builtin types in it.
    pub arbitrary_ints: bool,
    /// The standard prelude module the crate's modules import (`std::prelude::rust_2021`, or
    /// core's under `no_std`), `None` under `#![no_core]`.
    pub prelude: Option<ItemKey>,
}

/// A dependency of a crate, by the name it is known by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Dep<N> {
    pub name: N,
    pub krate: CrateItemKey,
    pub root: ItemKey,
}

// SAFETY: keys, and a name that is `Portable` when `N` is.
unsafe impl<N: Portable> Portable for Dep<N> {}

/// One crate's root record as a working form. [`CrateRootBuilder::write`] writes it as a
/// [`CrateRoot`] record and [`CrateRootBuilder::from`] reads it back, equal when its lists were
/// sorted as the record sorts them.
#[derive(Clone, Debug)]
pub struct CrateRootBuilder<'a, N> {
    pub head: CrateRootHead,
    /// The extern prelude, sorted by name: the `--extern` crates and the root's `extern crate`
    /// items (`extern crate self as name` too), each to its root module.
    pub extern_prelude: AVec<'a, (N, ItemKey)>,
    /// Every dependency by name, sorted: what an `extern crate` and a `$crate` resolve against.
    pub deps: AVec<'a, Dep<N>>,
    /// The root modules of the crates under `#[macro_use] extern crate` (std's or core's
    /// injected one included), whose exported macros are the `#[macro_use]` prelude, in order.
    pub macro_use: AVec<'a, ItemKey>,
    /// The crate's `#[macro_export]` `macro_rules!` written in source (not macro-expanded),
    /// sorted by name: they are bindings of the root's scope, each with its call shapes.
    pub macro_exports: AVec<'a, (N, ItemKey, MacroShapes)>,
    /// The tool names (`rustfmt`, `clippy`, `register_tool`'s), sorted: an attribute path
    /// beginning with one is inert.
    pub tools: AVec<'a, N>,
    /// The toolchain's built-in attribute names (`inline`, `test`, `derive`, ...), sorted:
    /// a one-segment attribute naming one is inert, and an attribute macro of a nearer scope
    /// shadowing one is ambiguous (rustc's built-in attribute scope).
    pub builtin_attrs: AVec<'a, N>,
    /// The language features the root turns on (`#![feature(default_field_values)]`), sorted:
    /// what a gate of the crate's code reads (front2's `field_default_allowed`).
    pub features: AVec<'a, N>,
}

impl<'a, N> CrateRootBuilder<'a, N> {
    /// A crate of `head` with empty lists.
    pub fn new_in(head: CrateRootHead, alloc: Alloc<'a>) -> CrateRootBuilder<'a, N> {
        CrateRootBuilder {
            head,
            extern_prelude: AVec::new_in(alloc),
            deps: AVec::new_in(alloc),
            macro_use: AVec::new_in(alloc),
            macro_exports: AVec::new_in(alloc),
            tools: AVec::new_in(alloc),
            builtin_attrs: AVec::new_in(alloc),
            features: AVec::new_in(alloc),
        }
    }
}

impl<'b, N: PartialEq> PartialEq<CrateRootBuilder<'b, N>> for CrateRootBuilder<'_, N> {
    fn eq(&self, other: &CrateRootBuilder<'b, N>) -> bool {
        self.head == other.head
            && self.extern_prelude[..] == other.extern_prelude[..]
            && self.deps[..] == other.deps[..]
            && self.macro_use[..] == other.macro_use[..]
            && self.macro_exports[..] == other.macro_exports[..]
            && self.tools[..] == other.tools[..]
            && self.builtin_attrs[..] == other.builtin_attrs[..]
            && self.features[..] == other.features[..]
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct NamedKeyRow: 1 {
        pub(crate) name: Text,
        pub(crate) key: ContentId,
    }
}

blaze_format::record! {
    /// One `#[macro_export]` macro: its name, its key and the call shapes its arms take.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct ExportRow: 1 {
        pub(crate) name: Text,
        pub(crate) key: ContentId,
        pub(crate) shapes: MacroShapes,
        pub(crate) pad: u8,
        pub(crate) pad2: u16,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct DepRow: 1 {
        pub(crate) name: Text,
        pub(crate) krate: ContentId,
        pub(crate) root: ContentId,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    /// 4: `features`, the root's `#![feature(..)]` names (a version-3 record is refused by
    /// `open`, and its crate's names are made again).
    pub(crate) struct CrateRootRoot: 4 {
        pub(crate) krate: ContentId,
        pub(crate) root_module: ContentId,
        pub(crate) root_file: ContentId,
        pub(crate) prelude: ContentId,
        pub(crate) externs: Slice<NamedKeyRow>,
        pub(crate) deps: Slice<DepRow>,
        pub(crate) macro_use: Slice<ContentId>,
        pub(crate) macro_exports: Slice<ExportRow>,
        pub(crate) tools: Slice<Text>,
        pub(crate) builtin_attrs: Slice<Text>,
        pub(crate) features: Slice<Text>,
        pub(crate) strings: Slice<u8>,
        pub(crate) edition: Edition,
        pub(crate) no_std: bool,
        pub(crate) no_core: bool,
        pub(crate) arbitrary_ints: bool,
    }
}

const TABLE: &str = "CrateRoot";

/// The indexes of `rows` in the order of `key` (a name, then a key), an exact repeat kept
/// once; with `unique`, two rows of one name are refused as a repeat of that table.
fn sorted_order<'b, T>(
    rows: &[T],
    key: impl Fn(&T) -> (&str, ContentId),
    unique: Option<&'static str>,
    alloc: Alloc<'b>,
) -> Result<AVec<'b, usize>, BuildError> {
    let mut order = AVec::from_iter_in(0..rows.len(), alloc);
    order.sort_unstable_by(|a, b| key(&rows[*a]).cmp(&key(&rows[*b])));
    order.dedup_by(|a, b| key(&rows[*a]) == key(&rows[*b]));
    if let Some(table) = unique {
        if let Some(pair) = order.windows(2).find(|pair| key(&rows[pair[0]]).0 == key(&rows[pair[1]]).0) {
            return Err(BuildError::Twice { table, key: key(&rows[pair[1]]).1 });
        }
    }
    Ok(order)
}

impl<N: AsRef<str>> CrateRootBuilder<'_, N> {
    /// The [`CrateRoot`] record of this crate, in `alloc`. Two names of the extern prelude or
    /// of the dependencies that name different crates are refused; an exact repeat is kept once.
    pub fn write<'b>(&self, alloc: Alloc<'b>) -> Result<RecordBytes<'b>, BuildError> {
        let zero = ContentId([0; 32]);
        let mut strings = Strings::new_in(alloc);
        let named = |rows: &[(N, ItemKey)], unique, strings: &mut Strings<'b>| -> Result<AVec<'b, NamedKeyRow>, BuildError> {
            let order = sorted_order(rows, |(name, key)| (name.as_ref(), key.0), unique, alloc)?;
            let mut out = Vec::with_capacity_in(order.len(), alloc);
            for &at in order.iter() {
                let (name, key) = &rows[at];
                out.push(NamedKeyRow { name: strings.push(name.as_ref(), TABLE)?, key: key.0 });
            }
            Ok(out)
        };
        let externs = named(&self.extern_prelude, Some("CrateRoot extern prelude"), &mut strings)?;
        let order = sorted_order(&self.deps, |d| (d.name.as_ref(), d.krate.0), Some("CrateRoot deps"), alloc)?;
        let mut deps = Vec::with_capacity_in(order.len(), alloc);
        for &at in order.iter() {
            let d = &self.deps[at];
            deps.push(DepRow { name: strings.push(d.name.as_ref(), TABLE)?, krate: d.krate.0, root: d.root.0 });
        }
        let order = sorted_order(&self.macro_exports, |(name, key, _)| (name.as_ref(), key.0), None, alloc)?;
        let mut macro_exports = Vec::with_capacity_in(order.len(), alloc);
        for &at in order.iter() {
            let (name, key, shapes) = &self.macro_exports[at];
            let name = strings.push(name.as_ref(), TABLE)?;
            macro_exports.push(ExportRow { name, key: key.0, shapes: *shapes, pad: 0, pad2: 0 });
        }
        let names = |rows: &[N], strings: &mut Strings<'b>| -> Result<AVec<'b, Text>, BuildError> {
            let order = sorted_order(rows, |name| (name.as_ref(), zero), None, alloc)?;
            let mut out = Vec::with_capacity_in(order.len(), alloc);
            for &at in order.iter() {
                out.push(strings.push(rows[at].as_ref(), TABLE)?);
            }
            Ok(out)
        };
        let tools = names(&self.tools, &mut strings)?;
        let builtin_attrs = names(&self.builtin_attrs, &mut strings)?;
        let features = names(&self.features, &mut strings)?;
        let macro_use = Vec::from_iter_in(self.macro_use.iter().map(|key| key.0), alloc);

        let mut out = Writer::new(WriterOpts { schema: <CrateRootRoot as Record>::SCHEMA, alloc: alloc, capacity: None });
        let text = Column::push(&mut out, strings.bytes());
        let externs = Column::push(&mut out, &externs);
        let deps = Column::push(&mut out, &deps);
        let macro_use = Column::push(&mut out, &macro_use);
        let macro_exports = Column::push(&mut out, &macro_exports);
        let tools = Column::push(&mut out, &tools);
        let builtin_attrs = Column::push(&mut out, &builtin_attrs);
        let features = Column::push(&mut out, &features);
        let head = self.head;
        Ok(finish_with(out, |at| CrateRootRoot {
            krate: head.krate.0,
            root_module: head.root_module.0,
            root_file: head.root_file,
            prelude: key_word(head.prelude),
            externs: externs.slice(at, offset_of!(CrateRootRoot, externs)),
            deps: deps.slice(at, offset_of!(CrateRootRoot, deps)),
            macro_use: macro_use.slice(at, offset_of!(CrateRootRoot, macro_use)),
            macro_exports: macro_exports.slice(at, offset_of!(CrateRootRoot, macro_exports)),
            tools: tools.slice(at, offset_of!(CrateRootRoot, tools)),
            builtin_attrs: builtin_attrs.slice(at, offset_of!(CrateRootRoot, builtin_attrs)),
            features: features.slice(at, offset_of!(CrateRootRoot, features)),
            strings: text.slice(at, offset_of!(CrateRootRoot, strings)),
            edition: head.edition,
            no_std: head.no_std,
            no_core: head.no_core,
            arbitrary_ints: head.arbitrary_ints,
        }))
    }
}

impl<'a, 'b> CrateRootBuilder<'b, &'a str> {
    /// The working form of a [`CrateRoot`] record, its names borrowed from the record, in
    /// `alloc`.
    pub fn from(view: &CrateRootView<'a>, alloc: Alloc<'b>) -> CrateRootBuilder<'b, &'a str> {
        let mut out = CrateRootBuilder::new_in(view.head(), alloc);
        out.extern_prelude.extend(view.externs());
        out.deps.extend(view.deps());
        out.macro_use.extend(view.macro_use());
        out.macro_exports.extend(view.macro_exports());
        out.tools.extend(view.tools());
        out.builtin_attrs.extend(view.builtin_attrs());
        out.features.extend(view.features());
        out
    }
}

/// A [`CrateRoot`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct CrateRootView<'a> {
    head: CrateRootHead,
    externs: &'a [NamedKeyRow],
    deps: &'a [DepRow],
    macro_use: &'a [ContentId],
    macro_exports: &'a [ExportRow],
    tools: &'a [Text],
    builtin_attrs: &'a [Text],
    features: &'a [Text],
    strings: &'a str,
}

impl<'a> CrateRootView<'a> {
    /// The record in `buffer`, checked once: its layout, every name, and the order of each
    /// sorted list, which the lookups probe by.
    pub fn read(buffer: &'a [u8]) -> Result<CrateRootView<'a>, FormatError> {
        let root = blaze_format::open::<CrateRootRoot>(buffer)?;
        let strings = root.strings.as_str(buffer)?;
        let externs = root.externs.follow(buffer)?;
        let deps = root.deps.follow(buffer)?;
        let macro_exports = root.macro_exports.follow(buffer)?;
        let tools = root.tools.follow(buffer)?;
        let builtin_attrs = root.builtin_attrs.follow(buffer)?;
        let features = root.features.follow(buffer)?;
        for row in externs {
            check_text(strings, row.name, row, buffer)?;
        }
        for row in macro_exports {
            check_text(strings, row.name, row, buffer)?;
        }
        for row in deps {
            check_text(strings, row.name, row, buffer)?;
        }
        for text in tools.iter().chain(builtin_attrs).chain(features) {
            check_text(strings, *text, text, buffer)?;
        }
        check_increasing(externs, buffer, "extern prelude out of order or repeated", |row| text_of(strings, row.name))?;
        check_increasing(deps, buffer, "dependencies out of order or repeated", |row| text_of(strings, row.name))?;
        check_increasing(macro_exports, buffer, "macro exports out of order or repeated", |row| {
            (text_of(strings, row.name), row.key)
        })?;
        for names in [tools, builtin_attrs, features] {
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
                arbitrary_ints: root.arbitrary_ints,
                prelude: key_of_word(root.prelude),
            },
            externs,
            deps,
            macro_use: root.macro_use.follow(buffer)?,
            macro_exports,
            tools,
            builtin_attrs,
            features,
            strings,
        })
    }

    pub fn head(&self) -> CrateRootHead {
        self.head
    }

    /// The root module `name` names in the extern prelude, by one binary search.
    pub fn extern_crate(&self, name: &str) -> Option<ItemKey> {
        let strings = self.strings;
        let at = self.externs.binary_search_by(|row| text_of(strings, row.name).cmp(name)).ok()?;
        Some(ItemKey(self.externs[at].key))
    }

    /// The extern prelude, by name.
    pub fn externs(&self) -> impl ExactSizeIterator<Item = (&'a str, ItemKey)> + use<'a> {
        named_of(self.strings, self.externs)
    }

    /// The dependency known by `name`, by one binary search.
    pub fn dep(&self, name: &str) -> Option<Dep<&'a str>> {
        let strings = self.strings;
        let at = self.deps.binary_search_by(|row| text_of(strings, row.name).cmp(name)).ok()?;
        self.deps().nth(at)
    }

    /// Every dependency, by name.
    pub fn deps(&self) -> impl ExactSizeIterator<Item = Dep<&'a str>> + use<'a> {
        let strings = self.strings;
        self.deps.iter().map(move |row| Dep {
            name: text_of(strings, row.name),
            krate: CrateItemKey(row.krate),
            root: ItemKey(row.root),
        })
    }

    /// The `#[macro_use]` prelude's crates' root modules, in order.
    pub fn macro_use(&self) -> impl ExactSizeIterator<Item = ItemKey> + use<'a> {
        self.macro_use.iter().map(|key| ItemKey(*key))
    }

    /// The `#[macro_export]` macros written in source, by name.
    pub fn macro_exports(&self) -> impl ExactSizeIterator<Item = (&'a str, ItemKey, MacroShapes)> + use<'a> {
        let strings = self.strings;
        self.macro_exports.iter().map(move |row| (text_of(strings, row.name), ItemKey(row.key), row.shapes))
    }

    /// The tool names, sorted.
    pub fn tools(&self) -> impl ExactSizeIterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.tools.iter().map(move |text| text_of(strings, *text))
    }

    /// The built-in attribute names, sorted.
    pub fn builtin_attrs(&self) -> impl ExactSizeIterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.builtin_attrs.iter().map(move |text| text_of(strings, *text))
    }

    /// The language features the root turns on, sorted.
    pub fn features(&self) -> impl ExactSizeIterator<Item = &'a str> + use<'a> {
        let strings = self.strings;
        self.features.iter().map(move |text| text_of(strings, *text))
    }

    /// Whether the root turns on the language feature `name` (`#![feature(name)]`), by one
    /// binary search.
    pub fn has_feature(&self, name: &str) -> bool {
        let strings = self.strings;
        self.features.binary_search_by(|text| text_of(strings, *text).cmp(name)).is_ok()
    }
}

fn named_of<'a>(strings: &'a str, rows: &'a [NamedKeyRow]) -> impl ExactSizeIterator<Item = (&'a str, ItemKey)> + use<'a> {
    rows.iter().map(move |row| (text_of(strings, row.name), ItemKey(row.key)))
}
