//! What the machine reads of the module it resolves: [`ScopeFacts`], and [`RecordFacts`], its
//! implementation over the item layer's records read through [`Items`].
//!
//! The values it hands back are blaze-surface's working forms ([`DeclsBuilder`],
//! [`CrateRootBuilder`], [`ModuleInfo`]), the one home of each: a record decodes losslessly
//! into its working form, and the machine reads only that.
//!
//! Reads of another module's scope are not here: the machine yields them as asks
//! ([`crate::Read`]) and takes their answers, so it never computes another module's scope.

use core::fmt;
use core::hash::Hash;

use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{
    CrateRootBuilder, DeclsBuilder, ExpandedDecls, Items, ModuleInfo, NotBuilt, SurfaceError,
};

/// What a name is to the machine: a small value that compares, orders and hashes. Its order
/// must be the order of the names' text, since a scope is sorted by it: `&str` (what
/// [`RecordFacts`] gives) orders by its text, and so do the tests' `&'static str`. Its text is
/// read where a rule names words of the language (the builtin types).
pub trait Name: Copy + Ord + Hash + fmt::Debug + AsRef<str> {}

impl<T: Copy + Ord + Hash + fmt::Debug + AsRef<str>> Name for T {}

/// Everything the machine reads of the module it resolves and of its crate, each a record
/// decoded into its working form in the facts' own allocator (which outlives the borrow of the
/// facts the working form is read under). A missing record is the [`SurfaceError`] naming it,
/// which the machine stops on; [`ScopeFacts::expansion`]'s `Ok(None)` means the call has not
/// been expanded yet.
pub trait ScopeFacts {
    type Name: Name;

    /// The module's place in its crate: the head of its own `ModuleDecls` record.
    fn module(&self, module: ItemKey) -> Result<ModuleInfo<Self::Name>, SurfaceError>;

    /// What the module's own text declares: its `ModuleDecls` record.
    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<'_, Self::Name>, SurfaceError>;

    /// The crate's `CrateRoot` record.
    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootBuilder<'_, Self::Name>, SurfaceError>;

    /// What the call's expansion adds to its module, or `None` while it is not expanded.
    fn expansion(&self, call: ItemKey) -> Result<Option<ExpandedDecls<'_, Self::Name>>, SurfaceError>;

    /// The helper attributes derive `derive` declares (`#[proc_macro_derive(D, attributes(h))]`),
    /// pushed to `out` in any order. The cells ask it only of a derive call that a one-segment
    /// attribute on its item follows (their helper scope). Facts that do not hold a derive's
    /// helpers refuse it by name, never answer "none".
    fn derive_helpers(&self, derive: ItemKey, out: &mut AVec<'_, Self::Name>) -> Result<(), SurfaceError> {
        let _ = out;
        Err(NotBuilt { method: "derive_helpers", item: derive }.into())
    }

    /// The module's `mod` lines (each child, and whether it is `#[macro_use]`) pushed to `out`
    /// when the facts hold them without its text: a one-group solve's module tree record
    /// ([`crate::tree_record`]). `false`: they are its [`ScopeFacts::decls`]' `mods`.
    fn mod_lines(&self, module: ItemKey, out: &mut AVec<'_, (ItemKey, bool)>) -> bool {
        let _ = (module, out);
        false
    }
}

/// [`ScopeFacts`] over the records an [`Items`] reads: names are the records' string-table
/// text, borrowed for `'a`, and each record is decoded into `alloc`.
///
/// Where each fact comes from: [`ScopeFacts::module`] is the head of the module's own
/// `ModuleDecls` record (its crate, parent, name in the parent and `#[no_implicit_prelude]`,
/// written by the parent's `mod` line's task), [`ScopeFacts::decls`] that record's lists, and
/// [`ScopeFacts::crate_root`] the crate's `CrateRoot` record. An expansion has no record yet,
/// so [`ScopeFacts::expansion`] is [`NotBuilt`]: a module whose macro call resolves stops there
/// with that error, never on a guess that the call adds nothing.
pub struct RecordFacts<'a, 'b, I: Items> {
    pub items: &'a I,
    pub alloc: Alloc<'b>,
}

impl<'a, 'b, I: Items> RecordFacts<'a, 'b, I> {
    pub fn new_in(items: &'a I, alloc: Alloc<'b>) -> RecordFacts<'a, 'b, I> {
        RecordFacts { items, alloc }
    }
}

impl<'a, I: Items> ScopeFacts for RecordFacts<'a, '_, I> {
    type Name = &'a str;

    fn module(&self, module: ItemKey) -> Result<ModuleInfo<&'a str>, SurfaceError> {
        Ok(self.items.decls(module)?.info())
    }

    fn decls(&self, module: ItemKey) -> Result<DeclsBuilder<'_, &'a str>, SurfaceError> {
        Ok(DeclsBuilder::from(&self.items.decls(module)?, self.alloc))
    }

    fn crate_root(&self, krate: CrateItemKey) -> Result<CrateRootBuilder<'_, &'a str>, SurfaceError> {
        Ok(CrateRootBuilder::from(&self.items.crate_root(krate)?, self.alloc))
    }

    fn expansion(&self, call: ItemKey) -> Result<Option<ExpandedDecls<'_, &'a str>>, SurfaceError> {
        Err(NotBuilt { method: "expansion", item: call }.into())
    }
}
