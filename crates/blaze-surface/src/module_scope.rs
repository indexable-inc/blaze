//! [`ModuleScope`]: a module's resolved scope, read per name (docs/v2/plan/design/bottom.md,
//! "Name resolution without resolving a crate").
//!
//! It is found by its own `ModuleDecls` and is valid by its read list (read-addressed), never
//! keyed by the scopes it imports from. A reader asks one `(namespace, name)` ([`ScopeView::get`])
//! or the [`ExportClass`] digest a glob reads ([`ScopeView::exports`]), and its read list holds
//! exactly that, so a private fn added to a module moves no reader that did not ask for its name
//! and no glob outside it. The read list itself is the store's, not this record's.

use core::alloc::Allocator;
use core::mem::offset_of;

use blaze_format::{FormatError, Slice, Writer};
use blaze_id::{ContentId, ItemKey};

use crate::column::{Column, Strings, Text, check_increasing, check_text, finish_with, text_of};
use crate::error::BuildError;
use crate::kind::{ExportClass, Namespace, Origin, Target, TargetTag, Visibility, VisibilityTag};

/// The domain of an `Exports` digest.
pub const EXPORTS_DOMAIN: &str = "blaze surface exports 1";

blaze_format::record! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct BindingRow: 1 {
        pub(crate) name: Text,
        pub(crate) target: ContentId,
        pub(crate) other: ContentId,
        pub(crate) restricted: ContentId,
        pub(crate) ns: Namespace,
        pub(crate) vis: VisibilityTag,
        pub(crate) origin: Origin,
        pub(crate) state: TargetTag,
    }
}

blaze_format::record! {
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ModuleScopeRoot: 1 {
        pub(crate) module: ContentId,
        /// By [`ExportClass`]'s byte.
        pub(crate) exports: [ContentId; 3],
        pub(crate) bindings: Slice<BindingRow>,
        pub(crate) fellows: Slice<ContentId>,
        pub(crate) strings: Slice<u8>,
    }
}

/// One binding of a scope: a name in a namespace, what it is bound to, who may name it through
/// this module, and how it came to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Binding<'a> {
    pub ns: Namespace,
    pub name: &'a str,
    pub target: Target<ItemKey>,
    pub vis: Visibility,
    pub origin: Origin,
}

/// The writer of a module's [`ModuleScope`]. Bindings are sorted by namespace and name at the
/// end, so the order they are given in moves no byte; two bindings of one slot are refused (the
/// resolver writes one, [`Target::Ambiguous`] when it must).
pub struct ModuleScope<A: Allocator + Clone> {
    module: ItemKey,
    rows: Vec<BindingRow, A>,
    fellows: Vec<ContentId, A>,
    staging: Strings<A>,
    alloc: A,
    /// The planted bug of the order test: the sort is skipped.
    #[cfg(test)]
    pub(crate) unsorted: bool,
}

impl<A: Allocator + Clone> ModuleScope<A> {
    pub fn new_in(module: ItemKey, alloc: A) -> ModuleScope<A> {
        ModuleScope {
            module,
            rows: Vec::new_in(alloc.clone()),
            fellows: Vec::new_in(alloc.clone()),
            staging: Strings::new_in(alloc.clone()),
            alloc,
            #[cfg(test)]
            unsorted: false,
        }
    }

    pub fn bind(&mut self, binding: Binding<'_>) -> Result<(), BuildError> {
        let name = self.staging.push(binding.name, "ModuleScope strings")?;
        let (vis, restricted) = binding.vis.to_record();
        let (state, target, other) = binding.target.to_record();
        self.rows.push(BindingRow {
            name,
            target,
            other,
            restricted,
            ns: binding.ns,
            vis,
            origin: binding.origin,
            state,
        });
        Ok(())
    }

    /// Names another module of the strongly connected set of globs this scope was solved in.
    pub fn fellow(&mut self, module: ItemKey) {
        self.fellows.push(module.0);
    }

    pub fn finish(mut self) -> Result<Vec<u8, A>, BuildError> {
        let staging = &self.staging;
        #[cfg(test)]
        let sort = !self.unsorted;
        #[cfg(not(test))]
        let sort = true;
        if sort {
            self.rows.sort_unstable_by(|a, b| (a.ns, staging.of(a.name)).cmp(&(b.ns, staging.of(b.name))));
            self.fellows.sort_unstable();
        }
        self.fellows.dedup();
        if let Some(pair) = self
            .rows
            .windows(2)
            .find(|pair| (pair[0].ns, staging.of(pair[0].name)) == (pair[1].ns, staging.of(pair[1].name)))
        {
            return Err(BuildError::Twice { table: "ModuleScope bindings", key: pair[1].target });
        }
        let mut strings = Strings::new_in(self.alloc.clone());
        for row in &mut self.rows {
            row.name = strings.moved(&self.staging, row.name, "ModuleScope strings")?;
        }
        let exports = [ExportClass::Pub, ExportClass::Crate, ExportClass::Within]
            .map(|class| exports_digest(&self.rows, &strings, class, self.alloc.clone()));
        let mut out =
            Writer::new_in(<ModuleScopeRoot as blaze_format::Record>::SCHEMA, self.alloc.clone());
        let text = Column::push(&mut out, strings.bytes());
        let bindings = Column::push(&mut out, &self.rows);
        let fellows = Column::push(&mut out, &self.fellows);
        Ok(finish_with(out, |at| ModuleScopeRoot {
            module: self.module.0,
            exports,
            bindings: bindings.slice(at, offset_of!(ModuleScopeRoot, bindings)),
            fellows: fellows.slice(at, offset_of!(ModuleScopeRoot, fellows)),
            strings: text.slice(at, offset_of!(ModuleScopeRoot, strings)),
        }))
    }
}

/// The digest of the bindings of `class`, in the record's order: everything a glob of that
/// class reads (namespace, name, visibility, origin, state and keys).
fn exports_digest<A: Allocator>(
    rows: &[BindingRow],
    strings: &Strings<A>,
    class: ExportClass,
    alloc: A,
) -> ContentId {
    let mut bytes = Vec::new_in(alloc);
    for row in rows {
        if !class.holds(Visibility::from_record(row.vis, row.restricted)) {
            continue;
        }
        let name = strings.of(row.name);
        bytes.push(row.ns.byte());
        bytes.extend_from_slice(&u64::try_from(name.len()).expect("a name fits a u64").to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&[row.vis.byte(), row.origin.byte(), row.state.byte()]);
        bytes.extend_from_slice(&row.restricted.0);
        bytes.extend_from_slice(&row.target.0);
        bytes.extend_from_slice(&row.other.0);
    }
    ContentId::of(EXPORTS_DOMAIN, &bytes)
}

/// A [`ModuleScope`] record read in place.
#[derive(Clone, Copy, Debug)]
pub struct ScopeView<'a> {
    module: ItemKey,
    exports: [ContentId; 3],
    rows: &'a [BindingRow],
    fellows: &'a [ContentId],
    strings: &'a str,
}

impl<'a> ScopeView<'a> {
    /// The record in `buffer`, checked once: its layout, every name, and that its bindings are
    /// in strictly increasing order of namespace and name, which [`ScopeView::get`] probes by.
    pub fn read(buffer: &'a [u8]) -> Result<ScopeView<'a>, FormatError> {
        let root = blaze_format::open::<ModuleScopeRoot>(buffer)?;
        let view = ScopeView {
            module: ItemKey(root.module),
            exports: root.exports,
            rows: root.bindings.follow(buffer)?,
            fellows: root.fellows.follow(buffer)?,
            strings: root.strings.as_str(buffer)?,
        };
        for row in view.rows {
            check_text(view.strings, row.name, row, buffer)?;
        }
        check_increasing(view.rows, buffer, "bindings out of order or two of one slot", |row| {
            (row.ns, text_of(view.strings, row.name))
        })?;
        check_increasing(view.fellows, buffer, "fellow modules out of order or repeated", |key| *key)?;
        Ok(view)
    }

    pub fn module(&self) -> ItemKey {
        self.module
    }

    fn decode(&self, row: &BindingRow) -> Binding<'a> {
        Binding {
            ns: row.ns,
            name: text_of(self.strings, row.name),
            target: Target::from_record(row.state, row.target, row.other),
            vis: Visibility::from_record(row.vis, row.restricted),
            origin: row.origin,
        }
    }

    /// The binding of `name` in `ns`, by one binary search.
    pub fn get(&self, ns: Namespace, name: &str) -> Option<Binding<'a>> {
        let strings = self.strings;
        let at = self
            .rows
            .binary_search_by(|row| (row.ns, text_of(strings, row.name)).cmp(&(ns, name)))
            .ok()?;
        Some(self.decode(&self.rows[at]))
    }

    /// Every binding, by namespace and name.
    pub fn bindings(&self) -> impl Iterator<Item = Binding<'a>> + use<'a> {
        let view = *self;
        self.rows.iter().map(move |row| view.decode(row))
    }

    /// The digest of the bindings a glob of `class` reads.
    pub fn exports(&self, class: ExportClass) -> ContentId {
        self.exports[usize::from(class.byte())]
    }

    /// The other modules of the strongly connected set this scope was solved in; none for a
    /// module alone.
    pub fn fellows(&self) -> impl Iterator<Item = ItemKey> + use<'a> {
        self.fellows.iter().map(|key| ItemKey(*key))
    }
}
